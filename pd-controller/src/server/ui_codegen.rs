use super::*;

pub(super) fn ui_block_catalog() -> Vec<UiBlockDefinition> {
    vec![
        UiBlockDefinition {
            id: "const_string",
            title: "Const String",
            category: "value",
            description: "Create a string variable output for downstream blocks.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "text_value",
                    placeholder: "text_value",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "hello",
                    placeholder: "hello",
                    connectable: false,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "const_number",
            title: "Const Number",
            category: "value",
            description: "Create a number variable output for downstream blocks.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "num_value",
                    placeholder: "num_value",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Number,
                    default_value: "1",
                    placeholder: "1",
                    connectable: false,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_header",
            title: "Get Header",
            category: "http_request",
            description: "Read request header into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "header",
                    placeholder: "header",
                    connectable: false,
                },
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "x-client-id",
                    placeholder: "x-client-id",
                    connectable: false,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_headers",
            title: "Get Request Headers",
            category: "http_request",
            description: "Read all request headers into a map variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_headers",
                placeholder: "request_headers",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_query_arg",
            title: "Get Query Arg",
            category: "http_request",
            description: "Read one query parameter value into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "query_value",
                    placeholder: "query_value",
                    connectable: false,
                },
                UiBlockInput {
                    key: "name",
                    label: "Query Name",
                    input_type: UiInputType::Text,
                    default_value: "id",
                    placeholder: "id",
                    connectable: false,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_query_args",
            title: "Get Query Args",
            category: "http_request",
            description: "Read all query parameters into a map variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "query_args",
                placeholder: "query_args",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_id",
            title: "Get Request ID",
            category: "http_request",
            description: "Read request id into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_id",
                placeholder: "request_id",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_method",
            title: "Get Request Method",
            category: "http_request",
            description: "Read request method into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_method",
                placeholder: "request_method",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_path",
            title: "Get Request Path",
            category: "http_request",
            description: "Read request path into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_path",
                placeholder: "request_path",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_query",
            title: "Get Request Query",
            category: "http_request",
            description: "Read request query into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_query",
                placeholder: "request_query",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_raw_query",
            title: "Get Raw Query",
            category: "http_request",
            description: "Read raw request query into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_raw_query",
                placeholder: "request_raw_query",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_path_with_query",
            title: "Get Path+Query",
            category: "http_request",
            description: "Read request path and query into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_path_with_query",
                placeholder: "request_path_with_query",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_scheme",
            title: "Get Request Scheme",
            category: "http_request",
            description: "Read request scheme into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_scheme",
                placeholder: "request_scheme",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_http_version",
            title: "Get HTTP Version",
            category: "http_request",
            description: "Read request HTTP version into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_http_version",
                placeholder: "request_http_version",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_port",
            title: "Get Request Port",
            category: "http_request",
            description: "Read request port into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_port",
                placeholder: "request_port",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_body",
            title: "Get Request Body",
            category: "http_request",
            description: "Read request body into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_body",
                placeholder: "request_body",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_host",
            title: "Get Request Host",
            category: "http_request",
            description: "Read request host into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "request_host",
                placeholder: "request_host",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_request_client_ip",
            title: "Get Client IP",
            category: "http_request",
            description: "Read request client ip into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "client_ip",
                placeholder: "client_ip",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "string_concat",
            title: "String Concat",
            category: "string",
            description: "Concatenate two string expressions into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "joined_text",
                    placeholder: "joined_text",
                    connectable: false,
                },
                UiBlockInput {
                    key: "left",
                    label: "Left",
                    input_type: UiInputType::Text,
                    default_value: "hello ",
                    placeholder: "hello or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "right",
                    label: "Right",
                    input_type: UiInputType::Text,
                    default_value: "world",
                    placeholder: "world or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "string_length",
            title: "String Length",
            category: "string",
            description: "Measure string length into a number variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "text_len",
                    placeholder: "text_len",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "hello",
                    placeholder: "hello or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "string_slice",
            title: "String Slice",
            category: "string",
            description: "Slice string by range and store into a variable (supports [:end], [start:], and negative end).",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "text_slice",
                    placeholder: "text_slice",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "hello",
                    placeholder: "hello or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "start",
                    label: "Start",
                    input_type: UiInputType::Number,
                    default_value: "0",
                    placeholder: "0, -1, or $var (empty for start)",
                    connectable: true,
                },
                UiBlockInput {
                    key: "end",
                    label: "End",
                    input_type: UiInputType::Number,
                    default_value: "3",
                    placeholder: "3, -1, or $var (empty for end)",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "math_add",
            title: "Math Add",
            category: "math",
            description: "Add two numbers and store the result in a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "math_sum",
                    placeholder: "math_sum",
                    connectable: false,
                },
                UiBlockInput {
                    key: "lhs",
                    label: "Left",
                    input_type: UiInputType::Number,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "rhs",
                    label: "Right",
                    input_type: UiInputType::Number,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "math_subtract",
            title: "Math Subtract",
            category: "math",
            description: "Subtract right number from left number.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "math_diff",
                    placeholder: "math_diff",
                    connectable: false,
                },
                UiBlockInput {
                    key: "lhs",
                    label: "Left",
                    input_type: UiInputType::Number,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "rhs",
                    label: "Right",
                    input_type: UiInputType::Number,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "math_multiply",
            title: "Math Multiply",
            category: "math",
            description: "Multiply two numbers and store the result.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "math_product",
                    placeholder: "math_product",
                    connectable: false,
                },
                UiBlockInput {
                    key: "lhs",
                    label: "Left",
                    input_type: UiInputType::Number,
                    default_value: "2",
                    placeholder: "2 or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "rhs",
                    label: "Right",
                    input_type: UiInputType::Number,
                    default_value: "2",
                    placeholder: "2 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "math_divide",
            title: "Math Divide",
            category: "math",
            description: "Divide left number by right number.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "math_quotient",
                    placeholder: "math_quotient",
                    connectable: false,
                },
                UiBlockInput {
                    key: "lhs",
                    label: "Left",
                    input_type: UiInputType::Number,
                    default_value: "4",
                    placeholder: "4 or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "rhs",
                    label: "Right",
                    input_type: UiInputType::Number,
                    default_value: "2",
                    placeholder: "2 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "array_new",
            title: "Array New",
            category: "collection",
            description: "Create an empty array variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "items",
                placeholder: "items",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "array_push",
            title: "Array Push",
            category: "collection",
            description: "Append a value to an array and output the updated array.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "items_next",
                    placeholder: "items_next",
                    connectable: false,
                },
                UiBlockInput {
                    key: "array",
                    label: "Array",
                    input_type: UiInputType::Text,
                    default_value: "$items",
                    placeholder: "$items",
                    connectable: true,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "item",
                    placeholder: "item or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "array_get",
            title: "Array Get",
            category: "collection",
            description: "Read array index into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "item_value",
                    placeholder: "item_value",
                    connectable: false,
                },
                UiBlockInput {
                    key: "array",
                    label: "Array",
                    input_type: UiInputType::Text,
                    default_value: "$items",
                    placeholder: "$items",
                    connectable: true,
                },
                UiBlockInput {
                    key: "index",
                    label: "Index",
                    input_type: UiInputType::Number,
                    default_value: "0",
                    placeholder: "0 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "array_set",
            title: "Array Set",
            category: "collection",
            description: "Set array index and output the updated array.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "items_next",
                    placeholder: "items_next",
                    connectable: false,
                },
                UiBlockInput {
                    key: "array",
                    label: "Array",
                    input_type: UiInputType::Text,
                    default_value: "$items",
                    placeholder: "$items",
                    connectable: true,
                },
                UiBlockInput {
                    key: "index",
                    label: "Index",
                    input_type: UiInputType::Number,
                    default_value: "0",
                    placeholder: "0 or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "item",
                    placeholder: "item or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "map_new",
            title: "Map New",
            category: "collection",
            description: "Create an empty map variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "map_value",
                placeholder: "map_value",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "map_get",
            title: "Map Get",
            category: "collection",
            description: "Read map key into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "map_item",
                    placeholder: "map_item",
                    connectable: false,
                },
                UiBlockInput {
                    key: "map",
                    label: "Map",
                    input_type: UiInputType::Text,
                    default_value: "$map_value",
                    placeholder: "$map_value",
                    connectable: true,
                },
                UiBlockInput {
                    key: "key",
                    label: "Key",
                    input_type: UiInputType::Text,
                    default_value: "key",
                    placeholder: "key or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "map_set",
            title: "Map Set",
            category: "collection",
            description: "Set map key and output the updated map.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "map_next",
                    placeholder: "map_next",
                    connectable: false,
                },
                UiBlockInput {
                    key: "map",
                    label: "Map",
                    input_type: UiInputType::Text,
                    default_value: "$map_value",
                    placeholder: "$map_value",
                    connectable: true,
                },
                UiBlockInput {
                    key: "key",
                    label: "Key",
                    input_type: UiInputType::Text,
                    default_value: "key",
                    placeholder: "key or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "value",
                    placeholder: "value or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "set_request_header",
            title: "Set Upstream Request Header",
            category: "http_upstream_request",
            description: "Set outbound request header via vm.http.upstream.request.set_header.",
            inputs: vec![
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "x-added",
                    placeholder: "x-added",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "add_request_header",
            title: "Add Upstream Request Header",
            category: "http_upstream_request",
            description: "Append outbound request header via vm.http.upstream.request.add_header.",
            inputs: vec![
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "x-added",
                    placeholder: "x-added",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "remove_request_header",
            title: "Remove Upstream Request Header",
            category: "http_upstream_request",
            description: "Remove outbound request header via vm.http.upstream.request.remove_header.",
            inputs: vec![UiBlockInput {
                key: "name",
                label: "Header Name",
                input_type: UiInputType::Text,
                default_value: "x-remove",
                placeholder: "x-remove",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "clear_request_header",
            title: "Clear Upstream Request Header",
            category: "http_upstream_request",
            description: "Clear outbound request header via vm.http.upstream.request.clear_header.",
            inputs: vec![UiBlockInput {
                key: "name",
                label: "Header Name",
                input_type: UiInputType::Text,
                default_value: "x-remove",
                placeholder: "x-remove",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_headers",
            title: "Set Upstream Request Headers",
            category: "http_upstream_request",
            description: "Set multiple outbound request headers from a map variable.",
            inputs: vec![UiBlockInput {
                key: "headers",
                label: "Headers",
                input_type: UiInputType::Text,
                default_value: "$request_headers",
                placeholder: "$request_headers",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_method",
            title: "Set Upstream Request Method",
            category: "http_upstream_request",
            description: "Set outbound request method via vm.http.upstream.request.set_method.",
            inputs: vec![UiBlockInput {
                key: "method",
                label: "Method",
                input_type: UiInputType::Text,
                default_value: "GET",
                placeholder: "GET or $var",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_path",
            title: "Set Upstream Request Path",
            category: "http_upstream_request",
            description: "Set outbound request path via vm.http.upstream.request.set_path.",
            inputs: vec![UiBlockInput {
                key: "path",
                label: "Path",
                input_type: UiInputType::Text,
                default_value: "/",
                placeholder: "/new-path or $var",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_query",
            title: "Set Upstream Request Query",
            category: "http_upstream_request",
            description: "Set outbound request query via vm.http.upstream.request.set_query.",
            inputs: vec![UiBlockInput {
                key: "query",
                label: "Query",
                input_type: UiInputType::Text,
                default_value: "x=1",
                placeholder: "x=1 or $var",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_raw_query",
            title: "Set Upstream Raw Query",
            category: "http_upstream_request",
            description: "Set outbound request raw query via vm.http.upstream.request.set_raw_query.",
            inputs: vec![UiBlockInput {
                key: "query",
                label: "Query",
                input_type: UiInputType::Text,
                default_value: "x=1",
                placeholder: "x=1 or $var",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_query_arg",
            title: "Set Upstream Query Arg",
            category: "http_upstream_request",
            description: "Set one outbound request query argument.",
            inputs: vec![
                UiBlockInput {
                    key: "name",
                    label: "Query Name",
                    input_type: UiInputType::Text,
                    default_value: "id",
                    placeholder: "id",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "1",
                    placeholder: "1 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_request_body",
            title: "Set Upstream Request Body",
            category: "http_upstream_request",
            description: "Set outbound request body via vm.http.upstream.request.set_body.",
            inputs: vec![UiBlockInput {
                key: "value",
                label: "Body",
                input_type: UiInputType::Text,
                default_value: "payload",
                placeholder: "payload or $var",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_header",
            title: "Set Header",
            category: "http_response",
            description: "Set response header via vm.http.response.set_header.",
            inputs: vec![
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "x-vm",
                    placeholder: "x-vm",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "ok",
                    placeholder: "ok or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "add_response_header",
            title: "Add Response Header",
            category: "http_response",
            description: "Append response header via vm.http.response.add_header.",
            inputs: vec![
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "set-cookie",
                    placeholder: "set-cookie",
                    connectable: false,
                },
                UiBlockInput {
                    key: "value",
                    label: "Value",
                    input_type: UiInputType::Text,
                    default_value: "a=1",
                    placeholder: "a=1 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "remove_response_header",
            title: "Remove Response Header",
            category: "http_response",
            description: "Remove response header via vm.http.response.remove_header.",
            inputs: vec![UiBlockInput {
                key: "name",
                label: "Header Name",
                input_type: UiInputType::Text,
                default_value: "x-remove",
                placeholder: "x-remove",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "clear_response_header",
            title: "Clear Response Header",
            category: "http_response",
            description: "Clear response header via vm.http.response.clear_header.",
            inputs: vec![UiBlockInput {
                key: "name",
                label: "Header Name",
                input_type: UiInputType::Text,
                default_value: "x-remove",
                placeholder: "x-remove",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_response_headers",
            title: "Set Response Headers",
            category: "http_response",
            description: "Set multiple response headers from a map variable.",
            inputs: vec![UiBlockInput {
                key: "headers",
                label: "Headers",
                input_type: UiInputType::Text,
                default_value: "$response_headers",
                placeholder: "$response_headers",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_response_content",
            title: "Set Response Content",
            category: "http_response",
            description: "Short-circuit request with response content.",
            inputs: vec![UiBlockInput {
                key: "value",
                label: "Body",
                input_type: UiInputType::Text,
                default_value: "okkk",
                placeholder: "request allowed or $var",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "set_response_status",
            title: "Set Response Status",
            category: "http_response",
            description: "Set response status code (used for short-circuit and upstream responses).",
            inputs: vec![UiBlockInput {
                key: "status",
                label: "Status Code",
                input_type: UiInputType::Number,
                default_value: "429",
                placeholder: "200-599",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "get_response_status",
            title: "Get Response Status",
            category: "http_response",
            description: "Read response status into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "response_status",
                placeholder: "response_status",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_response_header",
            title: "Get Response Header",
            category: "http_response",
            description: "Read response header into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "response_header",
                    placeholder: "response_header",
                    connectable: false,
                },
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "x-vm",
                    placeholder: "x-vm",
                    connectable: false,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_response_headers",
            title: "Get Response Headers",
            category: "http_response",
            description: "Read all response headers into a map variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "response_headers",
                placeholder: "response_headers",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_response_body",
            title: "Get Response Body",
            category: "http_response",
            description: "Read response body into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "response_body",
                placeholder: "response_body",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_upstream_response_status",
            title: "Get Upstream Response Status",
            category: "http_upstream_response",
            description: "Read upstream response status into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "upstream_status",
                placeholder: "upstream_status",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_upstream_response_header",
            title: "Get Upstream Response Header",
            category: "http_upstream_response",
            description: "Read upstream response header into a variable.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "upstream_header",
                    placeholder: "upstream_header",
                    connectable: false,
                },
                UiBlockInput {
                    key: "name",
                    label: "Header Name",
                    input_type: UiInputType::Text,
                    default_value: "x-upstream",
                    placeholder: "x-upstream",
                    connectable: false,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_upstream_response_headers",
            title: "Get Upstream Response Headers",
            category: "http_upstream_response",
            description: "Read all upstream response headers into a map variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "upstream_headers",
                placeholder: "upstream_headers",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "get_upstream_response_body",
            title: "Get Upstream Response Body",
            category: "http_upstream_response",
            description: "Read upstream response body into a variable.",
            inputs: vec![UiBlockInput {
                key: "var",
                label: "Variable",
                input_type: UiInputType::Text,
                default_value: "upstream_body",
                placeholder: "upstream_body",
                connectable: false,
            }],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "set_upstream",
            title: "Set Upstream",
            category: "routing",
            description: "Forward request to upstream host:port via vm.http.upstream.request.set_target.",
            inputs: vec![UiBlockInput {
                key: "upstream",
                label: "Upstream",
                input_type: UiInputType::Text,
                default_value: "127.0.0.1:8088",
                placeholder: "127.0.0.1:8088",
                connectable: true,
            }],
            outputs: vec![UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            }],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "rate_limit_allow",
            title: "Rate Limit Allow",
            category: "control",
            description: "Evaluate vm.http.rate_limit.allow and store a boolean result.",
            inputs: vec![
                UiBlockInput {
                    key: "var",
                    label: "Variable",
                    input_type: UiInputType::Text,
                    default_value: "rate_allowed",
                    placeholder: "rate_allowed",
                    connectable: false,
                },
                UiBlockInput {
                    key: "key_expr",
                    label: "Key Expr",
                    input_type: UiInputType::Text,
                    default_value: "$header",
                    placeholder: "$header or literal",
                    connectable: true,
                },
                UiBlockInput {
                    key: "limit",
                    label: "Limit",
                    input_type: UiInputType::Number,
                    default_value: "3",
                    placeholder: "3 or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "window_seconds",
                    label: "Window Seconds",
                    input_type: UiInputType::Number,
                    default_value: "60",
                    placeholder: "60 or $var",
                    connectable: true,
                },
            ],
            outputs: vec![UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            }],
            accepts_flow: false,
        },
        UiBlockDefinition {
            id: "rate_limit_if_else",
            title: "Rate Limit If/Else",
            category: "control",
            description: "Use vm.http.rate_limit.allow and branch to allowed/blocked flow outputs.",
            inputs: vec![
                UiBlockInput {
                    key: "key_expr",
                    label: "Key Expr",
                    input_type: UiInputType::Text,
                    default_value: "$header",
                    placeholder: "$header or literal",
                    connectable: true,
                },
                UiBlockInput {
                    key: "limit",
                    label: "Limit",
                    input_type: UiInputType::Number,
                    default_value: "3",
                    placeholder: "3",
                    connectable: false,
                },
                UiBlockInput {
                    key: "window_seconds",
                    label: "Window Seconds",
                    input_type: UiInputType::Number,
                    default_value: "60",
                    placeholder: "60",
                    connectable: false,
                },
            ],
            outputs: vec![
                UiBlockOutput {
                    key: "allowed",
                    label: "allowed",
                    expr_from_input: None,
                },
                UiBlockOutput {
                    key: "blocked",
                    label: "blocked",
                    expr_from_input: None,
                },
            ],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "if",
            title: "If",
            category: "control",
            description: "Plain conditional compare with true/false flow outputs.",
            inputs: vec![
                UiBlockInput {
                    key: "lhs",
                    label: "LHS",
                    input_type: UiInputType::Text,
                    default_value: "left",
                    placeholder: "left or $var",
                    connectable: true,
                },
                UiBlockInput {
                    key: "rhs",
                    label: "RHS",
                    input_type: UiInputType::Text,
                    default_value: "right",
                    placeholder: "right or $var",
                    connectable: true,
                },
            ],
            outputs: vec![
                UiBlockOutput {
                    key: "true",
                    label: "true",
                    expr_from_input: None,
                },
                UiBlockOutput {
                    key: "false",
                    label: "false",
                    expr_from_input: None,
                },
            ],
            accepts_flow: true,
        },
        UiBlockDefinition {
            id: "loop",
            title: "Loop",
            category: "control",
            description: "Plain fixed-count loop with body/done flow outputs.",
            inputs: vec![UiBlockInput {
                key: "count",
                label: "Count",
                input_type: UiInputType::Number,
                default_value: "1",
                placeholder: "1 or $var",
                connectable: true,
            }],
            outputs: vec![
                UiBlockOutput {
                    key: "body",
                    label: "body",
                    expr_from_input: None,
                },
                UiBlockOutput {
                    key: "done",
                    label: "done",
                    expr_from_input: None,
                },
            ],
            accepts_flow: true,
        },
    ]
}

#[derive(Clone, Debug)]
struct ResolvedUiNode {
    id: String,
    block: UiBlockInstance,
}

#[derive(Clone, Debug)]
struct ResolvedFlowEdge {
    source_output: String,
    target: String,
}

#[derive(Clone, Debug)]
struct ResolvedUiGraph {
    ordered_nodes: Vec<ResolvedUiNode>,
    flow_outgoing: HashMap<String, Vec<ResolvedFlowEdge>>,
    flow_incoming_count: HashMap<String, usize>,
    has_flow_edges: bool,
}

pub(super) fn render_ui_sources(
    blocks: &[UiBlockInstance],
    nodes: &[UiGraphNode],
    edges: &[UiGraphEdge],
) -> Result<UiSourceBundle, (StatusCode, Json<ErrorResponse>)> {
    if !blocks.is_empty() {
        return render_sources(blocks);
    }
    if nodes.is_empty() {
        return render_sources(&[]);
    }

    let resolved = resolve_ui_graph(nodes, edges)?;
    if !resolved.has_flow_edges {
        let ordered = resolved
            .ordered_nodes
            .into_iter()
            .map(|node| node.block)
            .collect::<Vec<_>>();
        return render_sources(&ordered);
    }
    render_sources_with_flow(&resolved)
}

fn resolve_ui_graph(
    nodes: &[UiGraphNode],
    edges: &[UiGraphEdge],
) -> Result<ResolvedUiGraph, (StatusCode, Json<ErrorResponse>)> {
    if nodes.len() > MAX_UI_BLOCKS {
        return Err(bad_request(&format!(
            "too many graph nodes: {} (limit {})",
            nodes.len(),
            MAX_UI_BLOCKS
        )));
    }

    let catalog = ui_block_catalog();
    let definition_map = catalog
        .iter()
        .map(|definition| (definition.id.to_string(), definition))
        .collect::<HashMap<_, _>>();
    let node_map = nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect::<HashMap<_, _>>();

    let mut data_incoming: HashMap<String, Vec<&UiGraphEdge>> = HashMap::new();
    let mut flow_outgoing: HashMap<String, Vec<ResolvedFlowEdge>> = HashMap::new();
    let mut flow_incoming_count: HashMap<String, usize> = HashMap::new();
    let mut indegree: HashMap<String, usize> = HashMap::new();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen_data_targets: HashSet<(String, String)> = HashSet::new();
    let mut seen_flow_outputs: HashSet<(String, String)> = HashSet::new();
    let mut has_flow_edges = false;

    for node in nodes {
        indegree.insert(node.id.clone(), 0);
        flow_incoming_count.insert(node.id.clone(), 0);
    }

    for edge in edges {
        let Some(source_node) = node_map.get(&edge.source) else {
            return Err(bad_request(&format!(
                "edge references missing source node '{}'",
                edge.source
            )));
        };
        let Some(target_node) = node_map.get(&edge.target) else {
            return Err(bad_request(&format!(
                "edge references missing target node '{}'",
                edge.target
            )));
        };

        let Some(source_definition) = definition_map.get(&source_node.block_id) else {
            return Err(bad_request(&format!(
                "unknown block_id '{}' for source node '{}'",
                source_node.block_id, source_node.id
            )));
        };
        let Some(source_output) = source_definition
            .outputs
            .iter()
            .find(|output| output.key == edge.source_output)
        else {
            return Err(bad_request(&format!(
                "source output '{}' not found on block '{}'",
                edge.source_output, source_definition.id
            )));
        };

        let Some(target_definition) = definition_map.get(&target_node.block_id) else {
            return Err(bad_request(&format!(
                "unknown block_id '{}' for target node '{}'",
                target_node.block_id, target_node.id
            )));
        };

        match source_output.expr_from_input {
            Some(_) => {
                let Some(target_input) = target_definition
                    .inputs
                    .iter()
                    .find(|input| input.key == edge.target_input)
                else {
                    return Err(bad_request(&format!(
                        "target input '{}' not found on block '{}'",
                        edge.target_input, target_definition.id
                    )));
                };
                if !target_input.connectable {
                    return Err(bad_request(&format!(
                        "target input '{}' on block '{}' is not connectable",
                        edge.target_input, target_definition.id
                    )));
                }
                let data_target_key = (edge.target.clone(), edge.target_input.clone());
                if !seen_data_targets.insert(data_target_key) {
                    return Err(bad_request(&format!(
                        "target input '{}' on node '{}' has multiple data connections",
                        edge.target_input, edge.target
                    )));
                }
                data_incoming
                    .entry(edge.target.clone())
                    .or_default()
                    .push(edge);
            }
            None => {
                if edge.target_input != "__flow" {
                    return Err(bad_request(&format!(
                        "control output '{}' must connect to target_input='__flow'",
                        edge.source_output
                    )));
                }
                if !target_definition.accepts_flow {
                    return Err(bad_request(&format!(
                        "target block '{}' does not accept flow edges",
                        target_definition.id
                    )));
                }
                let flow_key = (edge.source.clone(), edge.source_output.clone());
                if !seen_flow_outputs.insert(flow_key) {
                    return Err(bad_request(&format!(
                        "source output '{}' on node '{}' already connected",
                        edge.source_output, edge.source
                    )));
                }
                flow_outgoing
                    .entry(edge.source.clone())
                    .or_default()
                    .push(ResolvedFlowEdge {
                        source_output: edge.source_output.clone(),
                        target: edge.target.clone(),
                    });
                let flow_incoming = flow_incoming_count.entry(edge.target.clone()).or_default();
                *flow_incoming += 1;
                if *flow_incoming > 1 {
                    return Err(bad_request(&format!(
                        "target node '{}' has multiple incoming flow edges",
                        edge.target
                    )));
                }
                has_flow_edges = true;
            }
        }

        *indegree.entry(edge.target.clone()).or_default() += 1;
        adjacency
            .entry(edge.source.clone())
            .or_default()
            .push(edge.target.clone());
    }

    let order_hint = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect::<HashMap<_, _>>();
    for targets in adjacency.values_mut() {
        targets.sort_by_key(|node_id| order_hint.get(node_id).copied().unwrap_or(usize::MAX));
    }

    let mut queue = nodes
        .iter()
        .filter(|node| indegree.get(&node.id).copied().unwrap_or(0) == 0)
        .map(|node| node.id.clone())
        .collect::<VecDeque<_>>();
    let mut ordered_ids = Vec::with_capacity(nodes.len());
    while let Some(node_id) = queue.pop_front() {
        ordered_ids.push(node_id.clone());
        if let Some(targets) = adjacency.get(&node_id) {
            for target_id in targets {
                if let Some(entry) = indegree.get_mut(target_id) {
                    *entry = entry.saturating_sub(1);
                    if *entry == 0 {
                        queue.push_back(target_id.clone());
                    }
                }
            }
        }
    }
    if ordered_ids.len() != nodes.len() {
        return Err(bad_request(
            "graph contains a cycle; connect blocks as a directed acyclic graph",
        ));
    }

    for outgoing in flow_outgoing.values_mut() {
        outgoing.sort_by_key(|edge| order_hint.get(&edge.target).copied().unwrap_or(usize::MAX));
    }

    let mut ordered_nodes = Vec::with_capacity(nodes.len());
    for node_id in ordered_ids {
        let node = node_map
            .get(&node_id)
            .ok_or_else(|| bad_request("failed to resolve graph node"))?;
        let mut values = node.values.clone();

        if let Some(node_incoming) = data_incoming.get(&node_id) {
            for edge in node_incoming {
                let source_node = node_map
                    .get(&edge.source)
                    .ok_or_else(|| bad_request("failed to resolve edge source"))?;
                let source_definition = definition_map
                    .get(&source_node.block_id)
                    .ok_or_else(|| bad_request("failed to resolve source block definition"))?;
                let source_output = source_definition
                    .outputs
                    .iter()
                    .find(|output| output.key == edge.source_output)
                    .ok_or_else(|| bad_request("source output handle no longer exists"))?;
                let Some(expr_key) = source_output.expr_from_input else {
                    return Err(bad_request("source output does not expose an expression"));
                };
                let expr_name = source_node
                    .values
                    .get(expr_key)
                    .map(String::as_str)
                    .unwrap_or("value");
                let ident = sanitize_identifier(Some(&expr_name.to_string()), "value");
                values.insert(edge.target_input.clone(), format!("${ident}"));
            }
        }

        ordered_nodes.push(ResolvedUiNode {
            id: node.id.clone(),
            block: UiBlockInstance {
                block_id: node.block_id.clone(),
                values,
            },
        });
    }

    Ok(ResolvedUiGraph {
        ordered_nodes,
        flow_outgoing,
        flow_incoming_count,
        has_flow_edges,
    })
}

fn render_sources_with_flow(
    graph: &ResolvedUiGraph,
) -> Result<UiSourceBundle, (StatusCode, Json<ErrorResponse>)> {
    let mut rss_lines = vec!["use vm;".to_string(), String::new()];
    let mut js_lines = vec!["import * as vm from \"vm\";".to_string(), String::new()];
    let mut lua_lines = vec!["local vm = require(\"vm\")".to_string(), String::new()];
    let mut scm_lines = vec![
        "(require (prefix-in vm. \"vm\"))".to_string(),
        String::new(),
    ];

    let mut order_index = HashMap::new();
    let mut node_map = HashMap::new();
    for (index, node) in graph.ordered_nodes.iter().enumerate() {
        order_index.insert(node.id.clone(), index);
        node_map.insert(node.id.clone(), &node.block);
    }

    for node in &graph.ordered_nodes {
        if is_value_block(&node.block.block_id) {
            render_single_block(
                &node.block,
                &mut rss_lines,
                &mut js_lines,
                &mut lua_lines,
                &mut scm_lines,
            )?;
        }
    }

    for node in &graph.ordered_nodes {
        if is_value_block(&node.block.block_id) {
            continue;
        }
        if !is_flow_block(&node.block.block_id) {
            return Err(bad_request(&format!(
                "block '{}' is not flow-compatible when control edges are present",
                node.block.block_id
            )));
        }
    }

    let mut roots = graph
        .ordered_nodes
        .iter()
        .filter(|node| {
            !is_value_block(&node.block.block_id)
                && graph
                    .flow_incoming_count
                    .get(&node.id)
                    .copied()
                    .unwrap_or(0)
                    == 0
        })
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    roots.sort_by_key(|node_id| order_index.get(node_id).copied().unwrap_or(usize::MAX));

    let mut rendered = HashSet::new();
    let mut visiting = HashSet::new();
    for root in roots {
        let statements =
            render_flow_node(&root, graph, &node_map, &mut rendered, &mut visiting, 0)?;
        rss_lines.extend(statements.rustscript);
        js_lines.extend(statements.javascript);
        lua_lines.extend(statements.lua);
        scm_lines.extend(statements.scheme);
    }

    Ok(UiSourceBundle {
        rustscript: join_lines(&rss_lines),
        javascript: join_lines(&js_lines),
        lua: join_lines(&lua_lines),
        scheme: join_lines(&scm_lines),
    })
}

fn is_value_block(block_id: &str) -> bool {
    matches!(
        block_id,
        "const_string"
            | "const_number"
            | "get_header"
            | "get_request_headers"
            | "get_request_query_arg"
            | "get_request_query_args"
            | "get_request_id"
            | "get_request_method"
            | "get_request_path"
            | "get_request_query"
            | "get_request_raw_query"
            | "get_request_path_with_query"
            | "get_request_scheme"
            | "get_request_host"
            | "get_request_http_version"
            | "get_request_port"
            | "get_request_client_ip"
            | "get_request_body"
            | "get_response_status"
            | "get_response_header"
            | "get_response_headers"
            | "get_response_body"
            | "get_upstream_response_status"
            | "get_upstream_response_header"
            | "get_upstream_response_headers"
            | "get_upstream_response_body"
            | "string_concat"
            | "string_length"
            | "string_slice"
            | "math_add"
            | "math_subtract"
            | "math_multiply"
            | "math_divide"
            | "array_new"
            | "array_push"
            | "array_get"
            | "array_set"
            | "map_new"
            | "map_get"
            | "map_set"
            | "rate_limit_allow"
    )
}

fn is_flow_block(block_id: &str) -> bool {
    matches!(
        block_id,
        "set_request_header"
            | "add_request_header"
            | "remove_request_header"
            | "clear_request_header"
            | "set_request_headers"
            | "set_request_method"
            | "set_request_path"
            | "set_request_query"
            | "set_request_raw_query"
            | "set_request_query_arg"
            | "set_request_body"
            | "set_header"
            | "add_response_header"
            | "remove_response_header"
            | "clear_response_header"
            | "set_response_headers"
            | "set_response_content"
            | "set_response_status"
            | "set_upstream"
            | "rate_limit_if_else"
            | "if"
            | "loop"
    )
}

#[derive(Default)]
struct FlowStatements {
    rustscript: Vec<String>,
    javascript: Vec<String>,
    lua: Vec<String>,
    scheme: Vec<String>,
}

impl FlowStatements {
    fn extend(&mut self, mut other: FlowStatements) {
        self.rustscript.append(&mut other.rustscript);
        self.javascript.append(&mut other.javascript);
        self.lua.append(&mut other.lua);
        self.scheme.append(&mut other.scheme);
    }
}

fn render_flow_node(
    node_id: &str,
    graph: &ResolvedUiGraph,
    node_map: &HashMap<String, &UiBlockInstance>,
    rendered: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
    indent: usize,
) -> Result<FlowStatements, (StatusCode, Json<ErrorResponse>)> {
    if rendered.contains(node_id) {
        return Ok(FlowStatements::default());
    }

    if !visiting.insert(node_id.to_string()) {
        return Err(bad_request(
            "flow graph contains a cycle; use loop blocks instead of back edges",
        ));
    }

    let block = node_map
        .get(node_id)
        .ok_or_else(|| bad_request("flow path references missing node"))?;

    let result = match block.block_id.as_str() {
        "set_request_header"
        | "add_request_header"
        | "remove_request_header"
        | "clear_request_header"
        | "set_request_headers"
        | "set_request_method"
        | "set_request_path"
        | "set_request_query"
        | "set_request_raw_query"
        | "set_request_query_arg"
        | "set_request_body"
        | "set_header"
        | "add_response_header"
        | "remove_response_header"
        | "clear_response_header"
        | "set_response_headers"
        | "set_response_content"
        | "set_response_status"
        | "set_upstream" => {
            let mut statements = FlowStatements::default();
            let action = flow_action_statement(block)?;
            statements
                .rustscript
                .push(indent_line(indent, action.rustscript));
            statements
                .javascript
                .push(indent_line(indent, action.javascript));
            statements.lua.push(indent_line(indent, action.lua));
            statements.scheme.push(action.scheme);

            if let Some(next_target) = next_flow_target(node_id, graph)? {
                statements.extend(render_flow_node(
                    &next_target,
                    graph,
                    node_map,
                    rendered,
                    visiting,
                    indent,
                )?);
            }

            Ok(statements)
        }
        "rate_limit_if_else" => {
            let key_expr = block_value(block, "key_expr", "$header");
            let limit = sanitize_number(block.values.get("limit"), "3");
            let window = sanitize_number(block.values.get("window_seconds"), "60");
            let allowed_target =
                required_flow_target(node_id, graph, "allowed", "rate_limit_if_else")?;
            let blocked_target =
                required_flow_target(node_id, graph, "blocked", "rate_limit_if_else")?;

            let allowed_branch = render_flow_node(
                &allowed_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;
            let blocked_branch = render_flow_node(
                &blocked_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;

            let mut statements = FlowStatements::default();
            statements.rustscript.push(indent_line(
                indent,
                format!(
                    "if vm::http::rate_limit::allow({}, {}, {}) {{",
                    render_expr_rss(key_expr),
                    limit,
                    window
                ),
            ));
            statements
                .rustscript
                .extend(allowed_branch.rustscript.clone());
            statements
                .rustscript
                .push(indent_line(indent, "} else {".to_string()));
            statements
                .rustscript
                .extend(blocked_branch.rustscript.clone());
            statements
                .rustscript
                .push(indent_line(indent, "}".to_string()));

            statements.javascript.push(indent_line(
                indent,
                format!(
                    "if (vm.http.rate_limit.allow({}, {}, {})) {{",
                    render_expr_js(key_expr),
                    limit,
                    window
                ),
            ));
            statements
                .javascript
                .extend(allowed_branch.javascript.clone());
            statements
                .javascript
                .push(indent_line(indent, "} else {".to_string()));
            statements
                .javascript
                .extend(blocked_branch.javascript.clone());
            statements
                .javascript
                .push(indent_line(indent, "}".to_string()));

            statements.lua.push(indent_line(
                indent,
                format!(
                    "if vm.http.rate_limit.allow({}, {}, {}) then",
                    render_expr_lua(key_expr),
                    limit,
                    window
                ),
            ));
            statements.lua.extend(allowed_branch.lua.clone());
            statements.lua.push(indent_line(indent, "else".to_string()));
            statements.lua.extend(blocked_branch.lua.clone());
            statements.lua.push(indent_line(indent, "end".to_string()));

            statements.scheme.push(format!(
                "(if (vm.http.rate_limit.allow {} {} {}) {} {})",
                render_expr_scheme(key_expr),
                limit,
                window,
                scheme_branch_expr(&allowed_branch.scheme),
                scheme_branch_expr(&blocked_branch.scheme)
            ));
            Ok(statements)
        }
        "if" => {
            let lhs = block_value(block, "lhs", "left");
            let rhs = block_value(block, "rhs", "right");
            let true_target = required_flow_target(node_id, graph, "true", "if")?;
            let false_target = optional_flow_target(node_id, graph, "false");

            let true_branch = render_flow_node(
                &true_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;
            let false_branch = if let Some(target) = false_target {
                Some(render_flow_node(
                    &target,
                    graph,
                    node_map,
                    rendered,
                    visiting,
                    indent + 1,
                )?)
            } else {
                None
            };

            let mut statements = FlowStatements::default();
            statements.rustscript.push(indent_line(
                indent,
                format!("if {} == {} {{", render_expr_rss(lhs), render_expr_rss(rhs)),
            ));
            statements.rustscript.extend(true_branch.rustscript.clone());
            if let Some(false_branch) = &false_branch {
                statements
                    .rustscript
                    .push(indent_line(indent, "} else {".to_string()));
                statements
                    .rustscript
                    .extend(false_branch.rustscript.clone());
            }
            statements
                .rustscript
                .push(indent_line(indent, "}".to_string()));

            statements.javascript.push(indent_line(
                indent,
                format!(
                    "if ({} === {}) {{",
                    render_expr_js(lhs),
                    render_expr_js(rhs)
                ),
            ));
            statements.javascript.extend(true_branch.javascript.clone());
            if let Some(false_branch) = &false_branch {
                statements
                    .javascript
                    .push(indent_line(indent, "} else {".to_string()));
                statements
                    .javascript
                    .extend(false_branch.javascript.clone());
            }
            statements
                .javascript
                .push(indent_line(indent, "}".to_string()));

            statements.lua.push(indent_line(
                indent,
                format!(
                    "if {} == {} then",
                    render_expr_lua(lhs),
                    render_expr_lua(rhs)
                ),
            ));
            statements.lua.extend(true_branch.lua.clone());
            if let Some(false_branch) = &false_branch {
                statements.lua.push(indent_line(indent, "else".to_string()));
                statements.lua.extend(false_branch.lua.clone());
            }
            statements.lua.push(indent_line(indent, "end".to_string()));

            let scheme_false = false_branch
                .as_ref()
                .map(|branch| scheme_branch_expr(&branch.scheme))
                .unwrap_or_else(|| "null".to_string());
            statements.scheme.push(format!(
                "(if (== {} {}) {} {})",
                render_expr_scheme(lhs),
                render_expr_scheme(rhs),
                scheme_branch_expr(&true_branch.scheme),
                scheme_false
            ));
            Ok(statements)
        }
        "loop" => {
            let count = render_number_expr(block_value(block, "count", "1"), "1");
            let body_target = required_flow_target(node_id, graph, "body", "loop")?;
            let done_target = required_flow_target(node_id, graph, "done", "loop")?;

            let body_branch = render_flow_node(
                &body_target,
                graph,
                node_map,
                rendered,
                visiting,
                indent + 1,
            )?;
            let done_branch =
                render_flow_node(&done_target, graph, node_map, rendered, visiting, indent)?;

            let mut statements = FlowStatements::default();
            statements.rustscript.push(indent_line(
                indent,
                format!("for (let i = 0; i < {count}; i = i + 1) {{"),
            ));
            statements.rustscript.extend(body_branch.rustscript.clone());
            statements
                .rustscript
                .push(indent_line(indent, "}".to_string()));
            statements.rustscript.extend(done_branch.rustscript.clone());

            statements.javascript.push(indent_line(
                indent,
                format!("for (let i = 0; i < {count}; i = i + 1) {{"),
            ));
            statements.javascript.extend(body_branch.javascript.clone());
            statements
                .javascript
                .push(indent_line(indent, "}".to_string()));
            statements.javascript.extend(done_branch.javascript.clone());

            statements
                .lua
                .push(indent_line(indent, format!("for i = 1, {count} do")));
            statements.lua.extend(body_branch.lua.clone());
            statements.lua.push(indent_line(indent, "end".to_string()));
            statements.lua.extend(done_branch.lua.clone());

            statements.scheme.push(format!(
                "(let loop ((i 0)) (if (< i {count}) (begin {} (loop (+ i 1))) 'done))",
                scheme_branch_expr(&body_branch.scheme)
            ));
            statements.scheme.extend(done_branch.scheme.clone());
            Ok(statements)
        }
        other => Err(bad_request(&format!(
            "unsupported flow node block '{}'",
            other
        ))),
    };

    visiting.remove(node_id);
    if result.is_ok() {
        rendered.insert(node_id.to_string());
    }
    result
}

fn indent_line(level: usize, line: String) -> String {
    format!("{}{}", "    ".repeat(level), line)
}

fn next_flow_target(
    node_id: &str,
    graph: &ResolvedUiGraph,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    let outgoing = graph
        .flow_outgoing
        .get(node_id)
        .cloned()
        .unwrap_or_default();
    if outgoing.is_empty() {
        return Ok(None);
    }
    if outgoing.len() != 1 || outgoing[0].source_output != "next" {
        return Err(bad_request(
            "action blocks can only use a single 'next' outgoing flow edge",
        ));
    }
    Ok(Some(outgoing[0].target.clone()))
}

fn required_flow_target(
    node_id: &str,
    graph: &ResolvedUiGraph,
    output: &str,
    block_id: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    graph
        .flow_outgoing
        .get(node_id)
        .and_then(|edges| {
            edges
                .iter()
                .find(|edge| edge.source_output == output)
                .map(|edge| edge.target.clone())
        })
        .ok_or_else(|| {
            bad_request(&format!(
                "{block_id} requires a '{output}' outgoing flow edge"
            ))
        })
}

fn optional_flow_target(node_id: &str, graph: &ResolvedUiGraph, output: &str) -> Option<String> {
    graph.flow_outgoing.get(node_id).and_then(|edges| {
        edges
            .iter()
            .find(|edge| edge.source_output == output)
            .map(|edge| edge.target.clone())
    })
}

#[derive(Clone, Debug)]
struct FlowActionStatement {
    rustscript: String,
    javascript: String,
    lua: String,
    scheme: String,
}

fn flow_action_statement(
    block: &UiBlockInstance,
) -> Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)> {
    match block.block_id.as_str() {
        "set_request_header" => {
            let name = block_value(block, "name", "x-added");
            let value = block_value(block, "value", "1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "add_request_header" => {
            let name = block_value(block, "name", "x-added");
            let value = block_value(block, "value", "1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::add_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.upstream.request.add_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.upstream.request.add_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.add_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "remove_request_header" => {
            let name = block_value(block, "name", "x-remove");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::remove_header({});",
                    rust_string(name)
                ),
                javascript: format!(
                    "vm.http.upstream.request.remove_header({});",
                    js_string(name)
                ),
                lua: format!(
                    "vm.http.upstream.request.remove_header({})",
                    lua_string(name)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.remove_header {})",
                    scheme_string(name)
                ),
            })
        }
        "clear_request_header" => {
            let name = block_value(block, "name", "x-remove");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::clear_header({});",
                    rust_string(name)
                ),
                javascript: format!(
                    "vm.http.upstream.request.clear_header({});",
                    js_string(name)
                ),
                lua: format!(
                    "vm.http.upstream.request.clear_header({})",
                    lua_string(name)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.clear_header {})",
                    scheme_string(name)
                ),
            })
        }
        "set_request_headers" => {
            let headers = block_value(block, "headers", "$request_headers");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_headers({});",
                    render_expr_rss(headers)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_headers({});",
                    render_expr_js(headers)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_headers({})",
                    render_expr_lua(headers)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_headers {})",
                    render_expr_scheme(headers)
                ),
            })
        }
        "set_request_method" => {
            let method = block_value(block, "method", "GET");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_method({});",
                    render_expr_rss(method)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_method({});",
                    render_expr_js(method)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_method({})",
                    render_expr_lua(method)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_method {})",
                    render_expr_scheme(method)
                ),
            })
        }
        "set_request_path" => {
            let path = block_value(block, "path", "/");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_path({});",
                    render_expr_rss(path)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_path({});",
                    render_expr_js(path)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_path({})",
                    render_expr_lua(path)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_path {})",
                    render_expr_scheme(path)
                ),
            })
        }
        "set_request_query" => {
            let query = block_value(block, "query", "x=1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_query({});",
                    render_expr_rss(query)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_query({});",
                    render_expr_js(query)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_query({})",
                    render_expr_lua(query)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_query {})",
                    render_expr_scheme(query)
                ),
            })
        }
        "set_request_raw_query" => {
            let query = block_value(block, "query", "x=1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_raw_query({});",
                    render_expr_rss(query)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_raw_query({});",
                    render_expr_js(query)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_raw_query({})",
                    render_expr_lua(query)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_raw_query {})",
                    render_expr_scheme(query)
                ),
            })
        }
        "set_request_query_arg" => {
            let name = block_value(block, "name", "id");
            let value = block_value(block, "value", "1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_query_arg({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_query_arg({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_query_arg({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_query_arg {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "set_request_body" => {
            let value = block_value(block, "value", "payload");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_body({});",
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_body({});",
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_body({})",
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_body {})",
                    render_expr_scheme(value)
                ),
            })
        }
        "set_header" => {
            let name = block_value(block, "name", "x-vm");
            let value = block_value(block, "value", "ok");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::response::set_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.response.set_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.response.set_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.response.set_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "add_response_header" => {
            let name = block_value(block, "name", "set-cookie");
            let value = block_value(block, "value", "a=1");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::response::add_header({}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                javascript: format!(
                    "vm.http.response.add_header({}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                lua: format!(
                    "vm.http.response.add_header({}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                scheme: format!(
                    "(vm.http.response.add_header {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            })
        }
        "remove_response_header" => {
            let name = block_value(block, "name", "x-remove");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::remove_header({});", rust_string(name)),
                javascript: format!("vm.http.response.remove_header({});", js_string(name)),
                lua: format!("vm.http.response.remove_header({})", lua_string(name)),
                scheme: format!("(vm.http.response.remove_header {})", scheme_string(name)),
            })
        }
        "clear_response_header" => {
            let name = block_value(block, "name", "x-remove");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::clear_header({});", rust_string(name)),
                javascript: format!("vm.http.response.clear_header({});", js_string(name)),
                lua: format!("vm.http.response.clear_header({})", lua_string(name)),
                scheme: format!("(vm.http.response.clear_header {})", scheme_string(name)),
            })
        }
        "set_response_headers" => {
            let headers = block_value(block, "headers", "$response_headers");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::response::set_headers({});",
                    render_expr_rss(headers)
                ),
                javascript: format!("vm.http.response.set_headers({});", render_expr_js(headers)),
                lua: format!("vm.http.response.set_headers({})", render_expr_lua(headers)),
                scheme: format!(
                    "(vm.http.response.set_headers {})",
                    render_expr_scheme(headers)
                ),
            })
        }
        "set_response_content" => {
            let value = block_value(block, "value", "request allowed");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::set_body({});", render_expr_rss(value)),
                javascript: format!("vm.http.response.set_body({});", render_expr_js(value)),
                lua: format!("vm.http.response.set_body({})", render_expr_lua(value)),
                scheme: format!("(vm.http.response.set_body {})", render_expr_scheme(value)),
            })
        }
        "set_response_status" => {
            let status = sanitize_status_code(block.values.get("status"), "429");
            Ok(FlowActionStatement {
                rustscript: format!("vm::http::response::set_status({status});"),
                javascript: format!("vm.http.response.set_status({status});"),
                lua: format!("vm.http.response.set_status({status})"),
                scheme: format!("(vm.http.response.set_status {status})"),
            })
        }
        "set_upstream" => {
            let upstream = block_value(block, "upstream", "127.0.0.1:8088");
            Ok(FlowActionStatement {
                rustscript: format!(
                    "vm::http::upstream::request::set_target({});",
                    render_expr_rss(upstream)
                ),
                javascript: format!(
                    "vm.http.upstream.request.set_target({});",
                    render_expr_js(upstream)
                ),
                lua: format!(
                    "vm.http.upstream.request.set_target({})",
                    render_expr_lua(upstream)
                ),
                scheme: format!(
                    "(vm.http.upstream.request.set_target {})",
                    render_expr_scheme(upstream)
                ),
            })
        }
        other => Err(bad_request(&format!(
            "unsupported flow action block '{}'",
            other
        ))),
    }
}

fn scheme_branch_expr(expressions: &[String]) -> String {
    if expressions.is_empty() {
        return "null".to_string();
    }
    if expressions.len() == 1 {
        return expressions[0].clone();
    }
    format!("(begin {})", expressions.join(" "))
}

fn render_number_expr(raw: &str, fallback: &str) -> String {
    if let Some(expr) = raw.strip_prefix('$') {
        return sanitize_identifier(Some(&expr.to_string()), "value");
    }
    let trimmed = raw.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        trimmed.to_string()
    } else {
        fallback.to_string()
    }
}

fn is_signed_integer_literal(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    let digits = if let Some(rest) = raw.strip_prefix('-') {
        rest
    } else {
        raw
    };
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn render_slice_bound_expr(raw: Option<&String>) -> Option<String> {
    let raw = raw.map(String::as_str)?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(expr) = raw.strip_prefix('$') {
        return Some(sanitize_identifier(Some(&expr.to_string()), "value"));
    }
    if is_signed_integer_literal(raw) {
        return Some(raw.to_string());
    }
    None
}

fn render_slice_index_expr(raw: Option<&String>, fallback: &str) -> String {
    render_slice_bound_expr(raw).unwrap_or_else(|| fallback.to_string())
}

fn render_slice_expr(value_expr: String, start: Option<String>, end: Option<String>) -> String {
    match (start, end) {
        (Some(start), Some(end)) => format!("({value_expr})[{start}:{end}]"),
        (None, Some(end)) => format!("({value_expr})[:{end}]"),
        (Some(start), None) => format!("({value_expr})[{start}:]"),
        (None, None) => format!("({value_expr})[:]"),
    }
}

fn is_dot_member_key(raw: &str) -> bool {
    let Some(first) = raw.chars().next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    raw.chars()
        .skip(1)
        .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn map_key_member_name(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('$') || !is_dot_member_key(trimmed) {
        return None;
    }
    Some(trimmed)
}

fn render_key_access_expr(
    raw_container_expr: String,
    raw_key: &str,
    render_expr: fn(&str) -> String,
) -> String {
    if let Some(member) = map_key_member_name(raw_key) {
        format!("({raw_container_expr}).{member}")
    } else {
        format!("({raw_container_expr})[{}]", render_expr(raw_key))
    }
}

fn render_key_assignment_target(
    target_ident: &str,
    raw_key: &str,
    render_expr: fn(&str) -> String,
) -> String {
    if let Some(member) = map_key_member_name(raw_key) {
        format!("{target_ident}.{member}")
    } else {
        format!("{target_ident}[{}]", render_expr(raw_key))
    }
}

fn render_scheme_hash_key_expr(raw_key: &str) -> String {
    if let Some(member) = map_key_member_name(raw_key) {
        member.to_string()
    } else {
        render_expr_scheme(raw_key)
    }
}

fn render_sources(
    blocks: &[UiBlockInstance],
) -> Result<UiSourceBundle, (StatusCode, Json<ErrorResponse>)> {
    if blocks.len() > MAX_UI_BLOCKS {
        return Err(bad_request(&format!(
            "too many blocks: {} (limit {})",
            blocks.len(),
            MAX_UI_BLOCKS
        )));
    }

    let mut rss_lines = vec!["use vm;".to_string(), String::new()];
    let mut js_lines = vec!["import * as vm from \"vm\";".to_string(), String::new()];
    let mut lua_lines = vec!["local vm = require(\"vm\")".to_string(), String::new()];
    let mut scm_lines = vec![
        "(require (prefix-in vm. \"vm\"))".to_string(),
        String::new(),
    ];

    for block in blocks {
        render_single_block(
            block,
            &mut rss_lines,
            &mut js_lines,
            &mut lua_lines,
            &mut scm_lines,
        )?
    }

    Ok(UiSourceBundle {
        rustscript: join_lines(&rss_lines),
        javascript: join_lines(&js_lines),
        lua: join_lines(&lua_lines),
        scheme: join_lines(&scm_lines),
    })
}

fn render_single_block(
    block: &UiBlockInstance,
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match block.block_id.as_str() {
        "const_string" => {
            let var = sanitize_identifier(block.values.get("var"), "text_value");
            let value = block_value(block, "value", "hello");
            rss.push(format!("let {var} = {};", rust_string(value)));
            js.push(format!("let {var} = {};", js_string(value)));
            lua.push(format!("local {var} = {}", lua_string(value)));
            scm.push(format!("(define {var} {})", scheme_string(value)));
        }
        "const_number" => {
            let var = sanitize_identifier(block.values.get("var"), "num_value");
            let value = sanitize_number(block.values.get("value"), "1");
            rss.push(format!("let {var} = {value};"));
            js.push(format!("let {var} = {value};"));
            lua.push(format!("local {var} = {value}"));
            scm.push(format!("(define {var} {value})"));
        }
        "get_header" => {
            let var = sanitize_identifier(block.values.get("var"), "header");
            let header_name = block_value(block, "name", "x-client-id");
            rss.push(format!(
                "let {var} = vm::http::request::get_header({});",
                rust_string(header_name)
            ));
            js.push(format!(
                "let {var} = vm.http.request.get_header({});",
                js_string(header_name)
            ));
            lua.push(format!(
                "local {var} = vm.http.request.get_header({})",
                lua_string(header_name)
            ));
            scm.push(format!(
                "(define {var} (vm.http.request.get_header {}))",
                scheme_string(header_name)
            ));
        }
        "get_request_headers" => {
            let var = sanitize_identifier(block.values.get("var"), "request_headers");
            rss.push(format!("let {var} = vm::http::request::get_headers();"));
            js.push(format!("let {var} = vm.http.request.get_headers();"));
            lua.push(format!("local {var} = vm.http.request.get_headers()"));
            scm.push(format!("(define {var} (vm.http.request.get_headers))"));
        }
        "get_request_query_arg" => {
            let var = sanitize_identifier(block.values.get("var"), "query_value");
            let name = block_value(block, "name", "id");
            rss.push(format!(
                "let {var} = vm::http::request::get_query_arg({});",
                rust_string(name)
            ));
            js.push(format!(
                "let {var} = vm.http.request.get_query_arg({});",
                js_string(name)
            ));
            lua.push(format!(
                "local {var} = vm.http.request.get_query_arg({})",
                lua_string(name)
            ));
            scm.push(format!(
                "(define {var} (vm.http.request.get_query_arg {}))",
                scheme_string(name)
            ));
        }
        "get_request_query_args" => {
            let var = sanitize_identifier(block.values.get("var"), "query_args");
            rss.push(format!("let {var} = vm::http::request::get_query_args();"));
            js.push(format!("let {var} = vm.http.request.get_query_args();"));
            lua.push(format!("local {var} = vm.http.request.get_query_args()"));
            scm.push(format!("(define {var} (vm.http.request.get_query_args))"));
        }
        "get_request_id" => {
            let var = sanitize_identifier(block.values.get("var"), "request_id");
            rss.push(format!("let {var} = vm::http::request::get_id();"));
            js.push(format!("let {var} = vm.http.request.get_id();"));
            lua.push(format!("local {var} = vm.http.request.get_id()"));
            scm.push(format!("(define {var} (vm.http.request.get_id))"));
        }
        "get_request_method" => {
            let var = sanitize_identifier(block.values.get("var"), "request_method");
            rss.push(format!("let {var} = vm::http::request::get_method();"));
            js.push(format!("let {var} = vm.http.request.get_method();"));
            lua.push(format!("local {var} = vm.http.request.get_method()"));
            scm.push(format!("(define {var} (vm.http.request.get_method))"));
        }
        "get_request_path" => {
            let var = sanitize_identifier(block.values.get("var"), "request_path");
            rss.push(format!("let {var} = vm::http::request::get_path();"));
            js.push(format!("let {var} = vm.http.request.get_path();"));
            lua.push(format!("local {var} = vm.http.request.get_path()"));
            scm.push(format!("(define {var} (vm.http.request.get_path))"));
        }
        "get_request_query" => {
            let var = sanitize_identifier(block.values.get("var"), "request_query");
            rss.push(format!("let {var} = vm::http::request::get_query();"));
            js.push(format!("let {var} = vm.http.request.get_query();"));
            lua.push(format!("local {var} = vm.http.request.get_query()"));
            scm.push(format!("(define {var} (vm.http.request.get_query))"));
        }
        "get_request_raw_query" => {
            let var = sanitize_identifier(block.values.get("var"), "request_raw_query");
            rss.push(format!("let {var} = vm::http::request::get_raw_query();"));
            js.push(format!("let {var} = vm.http.request.get_raw_query();"));
            lua.push(format!("local {var} = vm.http.request.get_raw_query()"));
            scm.push(format!("(define {var} (vm.http.request.get_raw_query))"));
        }
        "get_request_path_with_query" => {
            let var = sanitize_identifier(block.values.get("var"), "request_path_with_query");
            rss.push(format!(
                "let {var} = vm::http::request::get_path_with_query();"
            ));
            js.push(format!(
                "let {var} = vm.http.request.get_path_with_query();"
            ));
            lua.push(format!(
                "local {var} = vm.http.request.get_path_with_query()"
            ));
            scm.push(format!(
                "(define {var} (vm.http.request.get_path_with_query))"
            ));
        }
        "get_request_scheme" => {
            let var = sanitize_identifier(block.values.get("var"), "request_scheme");
            rss.push(format!("let {var} = vm::http::request::get_scheme();"));
            js.push(format!("let {var} = vm.http.request.get_scheme();"));
            lua.push(format!("local {var} = vm.http.request.get_scheme()"));
            scm.push(format!("(define {var} (vm.http.request.get_scheme))"));
        }
        "get_request_host" => {
            let var = sanitize_identifier(block.values.get("var"), "request_host");
            rss.push(format!("let {var} = vm::http::request::get_host();"));
            js.push(format!("let {var} = vm.http.request.get_host();"));
            lua.push(format!("local {var} = vm.http.request.get_host()"));
            scm.push(format!("(define {var} (vm.http.request.get_host))"));
        }
        "get_request_http_version" => {
            let var = sanitize_identifier(block.values.get("var"), "request_http_version");
            rss.push(format!(
                "let {var} = vm::http::request::get_http_version();"
            ));
            js.push(format!("let {var} = vm.http.request.get_http_version();"));
            lua.push(format!("local {var} = vm.http.request.get_http_version()"));
            scm.push(format!("(define {var} (vm.http.request.get_http_version))"));
        }
        "get_request_port" => {
            let var = sanitize_identifier(block.values.get("var"), "request_port");
            rss.push(format!("let {var} = vm::http::request::get_port();"));
            js.push(format!("let {var} = vm.http.request.get_port();"));
            lua.push(format!("local {var} = vm.http.request.get_port()"));
            scm.push(format!("(define {var} (vm.http.request.get_port))"));
        }
        "get_request_client_ip" => {
            let var = sanitize_identifier(block.values.get("var"), "client_ip");
            rss.push(format!("let {var} = vm::http::request::get_client_ip();"));
            js.push(format!("let {var} = vm.http.request.get_client_ip();"));
            lua.push(format!("local {var} = vm.http.request.get_client_ip()"));
            scm.push(format!("(define {var} (vm.http.request.get_client_ip))"));
        }
        "get_request_body" => {
            let var = sanitize_identifier(block.values.get("var"), "request_body");
            rss.push(format!("let {var} = vm::http::request::get_body();"));
            js.push(format!("let {var} = vm.http.request.get_body();"));
            lua.push(format!("local {var} = vm.http.request.get_body()"));
            scm.push(format!("(define {var} (vm.http.request.get_body))"));
        }
        "get_response_status" => {
            let var = sanitize_identifier(block.values.get("var"), "response_status");
            rss.push(format!("let {var} = vm::http::response::get_status();"));
            js.push(format!("let {var} = vm.http.response.get_status();"));
            lua.push(format!("local {var} = vm.http.response.get_status()"));
            scm.push(format!("(define {var} (vm.http.response.get_status))"));
        }
        "get_response_header" => {
            let var = sanitize_identifier(block.values.get("var"), "response_header");
            let header_name = block_value(block, "name", "x-vm");
            rss.push(format!(
                "let {var} = vm::http::response::get_header({});",
                rust_string(header_name)
            ));
            js.push(format!(
                "let {var} = vm.http.response.get_header({});",
                js_string(header_name)
            ));
            lua.push(format!(
                "local {var} = vm.http.response.get_header({})",
                lua_string(header_name)
            ));
            scm.push(format!(
                "(define {var} (vm.http.response.get_header {}))",
                scheme_string(header_name)
            ));
        }
        "get_response_headers" => {
            let var = sanitize_identifier(block.values.get("var"), "response_headers");
            rss.push(format!("let {var} = vm::http::response::get_headers();"));
            js.push(format!("let {var} = vm.http.response.get_headers();"));
            lua.push(format!("local {var} = vm.http.response.get_headers()"));
            scm.push(format!("(define {var} (vm.http.response.get_headers))"));
        }
        "get_response_body" => {
            let var = sanitize_identifier(block.values.get("var"), "response_body");
            rss.push(format!("let {var} = vm::http::response::get_body();"));
            js.push(format!("let {var} = vm.http.response.get_body();"));
            lua.push(format!("local {var} = vm.http.response.get_body()"));
            scm.push(format!("(define {var} (vm.http.response.get_body))"));
        }
        "get_upstream_response_status" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_status");
            rss.push(format!(
                "let {var} = vm::http::upstream::response::get_status();"
            ));
            js.push(format!(
                "let {var} = vm.http.upstream.response.get_status();"
            ));
            lua.push(format!(
                "local {var} = vm.http.upstream.response.get_status()"
            ));
            scm.push(format!(
                "(define {var} (vm.http.upstream.response.get_status))"
            ));
        }
        "get_upstream_response_header" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_header");
            let header_name = block_value(block, "name", "x-upstream");
            rss.push(format!(
                "let {var} = vm::http::upstream::response::get_header({});",
                rust_string(header_name)
            ));
            js.push(format!(
                "let {var} = vm.http.upstream.response.get_header({});",
                js_string(header_name)
            ));
            lua.push(format!(
                "local {var} = vm.http.upstream.response.get_header({})",
                lua_string(header_name)
            ));
            scm.push(format!(
                "(define {var} (vm.http.upstream.response.get_header {}))",
                scheme_string(header_name)
            ));
        }
        "get_upstream_response_headers" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_headers");
            rss.push(format!(
                "let {var} = vm::http::upstream::response::get_headers();"
            ));
            js.push(format!(
                "let {var} = vm.http.upstream.response.get_headers();"
            ));
            lua.push(format!(
                "local {var} = vm.http.upstream.response.get_headers()"
            ));
            scm.push(format!(
                "(define {var} (vm.http.upstream.response.get_headers))"
            ));
        }
        "get_upstream_response_body" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_body");
            rss.push(format!(
                "let {var} = vm::http::upstream::response::get_body();"
            ));
            js.push(format!("let {var} = vm.http.upstream.response.get_body();"));
            lua.push(format!(
                "local {var} = vm.http.upstream.response.get_body()"
            ));
            scm.push(format!(
                "(define {var} (vm.http.upstream.response.get_body))"
            ));
        }
        "string_concat" => {
            let var = sanitize_identifier(block.values.get("var"), "joined_text");
            let left = block_value(block, "left", "hello ");
            let right = block_value(block, "right", "world");
            rss.push(format!(
                "let {var} = {} + {};",
                render_expr_rss(left),
                render_expr_rss(right)
            ));
            js.push(format!(
                "let {var} = {} + {};",
                render_expr_js(left),
                render_expr_js(right)
            ));
            lua.push(format!(
                "local {var} = {} + {}",
                render_expr_lua(left),
                render_expr_lua(right)
            ));
            scm.push(format!(
                "(define {var} (+ {} {}))",
                render_expr_scheme(left),
                render_expr_scheme(right)
            ));
        }
        "string_length" => {
            let var = sanitize_identifier(block.values.get("var"), "text_len");
            let value = block_value(block, "value", "hello");
            rss.push(format!("let {var} = len({});", render_expr_rss(value)));
            js.push(format!("let {var} = len({});", render_expr_js(value)));
            lua.push(format!("local {var} = len({})", render_expr_lua(value)));
            scm.push(format!(
                "(define {var} (len {}))",
                render_expr_scheme(value)
            ));
        }
        "string_slice" => {
            let var = sanitize_identifier(block.values.get("var"), "text_slice");
            let value = block_value(block, "value", "hello");
            let start = render_slice_bound_expr(block.values.get("start"));
            let end = render_slice_bound_expr(block.values.get("end"));
            // Compatibility for previously saved graphs that still use `length`.
            let legacy_length = if end.is_none() {
                render_slice_bound_expr(block.values.get("length"))
            } else {
                None
            };

            let rss_expr = if let Some(length) = &legacy_length {
                let legacy_start = start.clone().unwrap_or_else(|| "0".to_string());
                let legacy_end = format!("({legacy_start}) + ({length})");
                render_slice_expr(render_expr_rss(value), Some(legacy_start), Some(legacy_end))
            } else {
                render_slice_expr(render_expr_rss(value), start.clone(), end.clone())
            };
            let js_expr = if let Some(length) = &legacy_length {
                let legacy_start = start.clone().unwrap_or_else(|| "0".to_string());
                let legacy_end = format!("({legacy_start}) + ({length})");
                render_slice_expr(render_expr_js(value), Some(legacy_start), Some(legacy_end))
            } else {
                render_slice_expr(render_expr_js(value), start.clone(), end.clone())
            };
            let lua_expr = if let Some(length) = &legacy_length {
                let legacy_start = start.clone().unwrap_or_else(|| "0".to_string());
                let legacy_end = format!("({legacy_start}) + ({length})");
                render_slice_expr(render_expr_lua(value), Some(legacy_start), Some(legacy_end))
            } else {
                render_slice_expr(render_expr_lua(value), start.clone(), end.clone())
            };

            rss.push(format!("let {var} = {rss_expr};"));
            js.push(format!("let {var} = {js_expr};"));
            lua.push(format!("local {var} = {lua_expr}"));

            let scheme_value = render_expr_scheme(value);
            let scheme_expr = if let Some(length) = legacy_length {
                let legacy_start = start
                    .clone()
                    .unwrap_or_else(|| render_slice_index_expr(None, "0"));
                format!("(slice {scheme_value} {legacy_start} {length})")
            } else {
                match (start, end) {
                    (Some(start), Some(end)) => {
                        format!("(slice-range {scheme_value} {start} {end})")
                    }
                    (None, Some(end)) => format!("(slice-to {scheme_value} {end})"),
                    (Some(start), None) => format!("(slice-from {scheme_value} {start})"),
                    (None, None) => format!("(slice-from {scheme_value} 0)"),
                }
            };
            scm.push(format!("(define {var} {scheme_expr})"));
        }
        "math_add" => {
            let var = sanitize_identifier(block.values.get("var"), "math_sum");
            let lhs = render_number_expr(block_value(block, "lhs", "1"), "1");
            let rhs = render_number_expr(block_value(block, "rhs", "1"), "1");
            rss.push(format!("let {var} = {lhs} + {rhs};"));
            js.push(format!("let {var} = {lhs} + {rhs};"));
            lua.push(format!("local {var} = {lhs} + {rhs}"));
            scm.push(format!("(define {var} (+ {lhs} {rhs}))"));
        }
        "math_subtract" => {
            let var = sanitize_identifier(block.values.get("var"), "math_diff");
            let lhs = render_number_expr(block_value(block, "lhs", "1"), "1");
            let rhs = render_number_expr(block_value(block, "rhs", "1"), "1");
            rss.push(format!("let {var} = {lhs} - {rhs};"));
            js.push(format!("let {var} = {lhs} - {rhs};"));
            lua.push(format!("local {var} = {lhs} - {rhs}"));
            scm.push(format!("(define {var} (- {lhs} {rhs}))"));
        }
        "math_multiply" => {
            let var = sanitize_identifier(block.values.get("var"), "math_product");
            let lhs = render_number_expr(block_value(block, "lhs", "2"), "2");
            let rhs = render_number_expr(block_value(block, "rhs", "2"), "2");
            rss.push(format!("let {var} = {lhs} * {rhs};"));
            js.push(format!("let {var} = {lhs} * {rhs};"));
            lua.push(format!("local {var} = {lhs} * {rhs}"));
            scm.push(format!("(define {var} (* {lhs} {rhs}))"));
        }
        "math_divide" => {
            let var = sanitize_identifier(block.values.get("var"), "math_quotient");
            let lhs = render_number_expr(block_value(block, "lhs", "4"), "4");
            let rhs = render_number_expr(block_value(block, "rhs", "2"), "2");
            rss.push(format!("let {var} = {lhs} / {rhs};"));
            js.push(format!("let {var} = {lhs} / {rhs};"));
            lua.push(format!("local {var} = {lhs} / {rhs}"));
            scm.push(format!("(define {var} (/ {lhs} {rhs}))"));
        }
        "array_new" => {
            let var = sanitize_identifier(block.values.get("var"), "items");
            rss.push(format!("let {var} = [];"));
            js.push(format!("let {var} = [];"));
            lua.push(format!("local {var} = []"));
            scm.push(format!("(define {var} (vector))"));
        }
        "array_push" => {
            let var = sanitize_identifier(block.values.get("var"), "items_next");
            let array = block_value(block, "array", "$items");
            let value = block_value(block, "value", "item");
            rss.push(format!("let {var} = {};", render_expr_rss(array)));
            rss.push(format!("{var}[len({var})] = {};", render_expr_rss(value)));
            js.push(format!("let {var} = {};", render_expr_js(array)));
            js.push(format!("{var}[len({var})] = {};", render_expr_js(value)));
            lua.push(format!("local {var} = {}", render_expr_lua(array)));
            lua.push(format!("{var}[len({var})] = {}", render_expr_lua(value)));
            scm.push(format!("(define {var} {})", render_expr_scheme(array)));
            scm.push(format!(
                "(vector-set! {var} (len {var}) {})",
                render_expr_scheme(value)
            ));
        }
        "array_get" => {
            let var = sanitize_identifier(block.values.get("var"), "item_value");
            let array = block_value(block, "array", "$items");
            let index = render_number_expr(block_value(block, "index", "0"), "0");
            rss.push(format!(
                "let {var} = ({})[{index}];",
                render_expr_rss(array)
            ));
            js.push(format!("let {var} = ({})[{index}];", render_expr_js(array)));
            lua.push(format!(
                "local {var} = ({})[{index}]",
                render_expr_lua(array)
            ));
            scm.push(format!(
                "(define {var} (vector-ref {} {index}))",
                render_expr_scheme(array)
            ));
        }
        "array_set" => {
            let var = sanitize_identifier(block.values.get("var"), "items_next");
            let array = block_value(block, "array", "$items");
            let index = render_number_expr(block_value(block, "index", "0"), "0");
            let value = block_value(block, "value", "item");
            rss.push(format!("let {var} = {};", render_expr_rss(array)));
            rss.push(format!("{var}[{index}] = {};", render_expr_rss(value)));
            js.push(format!("let {var} = {};", render_expr_js(array)));
            js.push(format!("{var}[{index}] = {};", render_expr_js(value)));
            lua.push(format!("local {var} = {}", render_expr_lua(array)));
            lua.push(format!("{var}[{index}] = {}", render_expr_lua(value)));
            scm.push(format!("(define {var} {})", render_expr_scheme(array)));
            scm.push(format!(
                "(vector-set! {var} {index} {})",
                render_expr_scheme(value)
            ));
        }
        "map_new" => {
            let var = sanitize_identifier(block.values.get("var"), "map_value");
            rss.push(format!("let {var} = {{}};"));
            js.push(format!("let {var} = {{}};"));
            lua.push(format!("local {var} = {{}}"));
            scm.push(format!("(define {var} (hash))"));
        }
        "map_get" => {
            let var = sanitize_identifier(block.values.get("var"), "map_item");
            let map = block_value(block, "map", "$map_value");
            let key = block_value(block, "key", "key");
            rss.push(format!(
                "let {var} = {};",
                render_key_access_expr(render_expr_rss(map), key, render_expr_rss)
            ));
            js.push(format!(
                "let {var} = {};",
                render_key_access_expr(render_expr_js(map), key, render_expr_js)
            ));
            lua.push(format!(
                "local {var} = {}",
                render_key_access_expr(render_expr_lua(map), key, render_expr_lua)
            ));
            scm.push(format!(
                "(define {var} (hash-ref {} {}))",
                render_expr_scheme(map),
                render_scheme_hash_key_expr(key)
            ));
        }
        "map_set" => {
            let var = sanitize_identifier(block.values.get("var"), "map_next");
            let map = block_value(block, "map", "$map_value");
            let key = block_value(block, "key", "key");
            let value = block_value(block, "value", "value");
            rss.push(format!("let {var} = {};", render_expr_rss(map)));
            rss.push(format!(
                "{} = {};",
                render_key_assignment_target(&var, key, render_expr_rss),
                render_expr_rss(value)
            ));
            js.push(format!("let {var} = {};", render_expr_js(map)));
            js.push(format!(
                "{} = {};",
                render_key_assignment_target(&var, key, render_expr_js),
                render_expr_js(value)
            ));
            lua.push(format!("local {var} = {}", render_expr_lua(map)));
            lua.push(format!(
                "{} = {}",
                render_key_assignment_target(&var, key, render_expr_lua),
                render_expr_lua(value)
            ));
            scm.push(format!("(define {var} {})", render_expr_scheme(map)));
            scm.push(format!(
                "(hash-set! {var} {} {})",
                render_scheme_hash_key_expr(key),
                render_expr_scheme(value)
            ));
        }
        "set_request_header"
        | "add_request_header"
        | "remove_request_header"
        | "clear_request_header"
        | "set_request_headers"
        | "set_request_method"
        | "set_request_path"
        | "set_request_query"
        | "set_request_raw_query"
        | "set_request_query_arg"
        | "set_request_body"
        | "set_header"
        | "add_response_header"
        | "remove_response_header"
        | "clear_response_header"
        | "set_response_headers"
        | "set_response_content"
        | "set_response_status"
        | "set_upstream" => {
            let action = flow_action_statement(block)?;
            rss.push(action.rustscript);
            js.push(action.javascript);
            lua.push(action.lua);
            scm.push(action.scheme);
        }
        "rate_limit_allow" => {
            let var = sanitize_identifier(block.values.get("var"), "rate_allowed");
            let key_expr = block_value(block, "key_expr", "$header");
            let limit = render_number_expr(block_value(block, "limit", "3"), "3");
            let window = render_number_expr(block_value(block, "window_seconds", "60"), "60");

            rss.push(format!(
                "let {var} = vm::http::rate_limit::allow({}, {}, {});",
                render_expr_rss(key_expr),
                limit,
                window
            ));
            js.push(format!(
                "let {var} = vm.http.rate_limit.allow({}, {}, {});",
                render_expr_js(key_expr),
                limit,
                window
            ));
            lua.push(format!(
                "local {var} = vm.http.rate_limit.allow({}, {}, {})",
                render_expr_lua(key_expr),
                limit,
                window
            ));
            scm.push(format!(
                "(define {var} (vm.http.rate_limit.allow {} {} {}))",
                render_expr_scheme(key_expr),
                limit,
                window
            ));
        }
        "rate_limit_if_else" => {
            let key_expr = block_value(block, "key_expr", "$header");
            let limit = sanitize_number(block.values.get("limit"), "3");
            let window = sanitize_number(block.values.get("window_seconds"), "60");

            rss.push(format!(
                "if vm::http::rate_limit::allow({}, {}, {}) {{",
                render_expr_rss(key_expr),
                limit,
                window
            ));
            rss.push(format!(
                "    vm::http::response::set_body({});",
                rust_string("request allowed")
            ));
            rss.push("} else {".to_string());
            rss.push(format!(
                "    vm::http::response::set_body({});",
                rust_string("rate limit exceeded")
            ));
            rss.push("}".to_string());

            js.push(format!(
                "if (vm.http.rate_limit.allow({}, {}, {})) {{",
                render_expr_js(key_expr),
                limit,
                window
            ));
            js.push(format!(
                "    vm.http.response.set_body({});",
                js_string("request allowed")
            ));
            js.push("} else {".to_string());
            js.push(format!(
                "    vm.http.response.set_body({});",
                js_string("rate limit exceeded")
            ));
            js.push("}".to_string());

            lua.push(format!(
                "if vm.http.rate_limit.allow({}, {}, {}) then",
                render_expr_lua(key_expr),
                limit,
                window
            ));
            lua.push(format!(
                "    vm.http.response.set_body({})",
                lua_string("request allowed")
            ));
            lua.push("else".to_string());
            lua.push(format!(
                "    vm.http.response.set_body({})",
                lua_string("rate limit exceeded")
            ));
            lua.push("end".to_string());

            scm.push(format!(
                "(if (vm.http.rate_limit.allow {} {} {})",
                render_expr_scheme(key_expr),
                limit,
                window
            ));
            scm.push(format!(
                "    (vm.http.response.set_body {})",
                scheme_string("request allowed")
            ));
            scm.push(format!(
                "    (vm.http.response.set_body {}))",
                scheme_string("rate limit exceeded")
            ));
        }
        "if" => {
            let lhs = block_value(block, "lhs", "left");
            let rhs = block_value(block, "rhs", "right");

            rss.push(format!(
                "if {} == {} {{",
                render_expr_rss(lhs),
                render_expr_rss(rhs)
            ));
            rss.push("} else {".to_string());
            rss.push("}".to_string());

            js.push(format!(
                "if ({} === {}) {{",
                render_expr_js(lhs),
                render_expr_js(rhs)
            ));
            js.push("} else {".to_string());
            js.push("}".to_string());

            lua.push(format!(
                "if {} == {} then",
                render_expr_lua(lhs),
                render_expr_lua(rhs)
            ));
            lua.push("else".to_string());
            lua.push("end".to_string());

            scm.push(format!(
                "(if (== {} {}) null null)",
                render_expr_scheme(lhs),
                render_expr_scheme(rhs)
            ));
        }
        "loop" => {
            let count = render_number_expr(block_value(block, "count", "1"), "1");

            rss.push(format!("for (let i = 0; i < {count}; i = i + 1) {{"));
            rss.push("}".to_string());

            js.push(format!("for (let i = 0; i < {count}; i = i + 1) {{"));
            js.push("}".to_string());

            lua.push(format!("for i = 1, {count} do"));
            lua.push("end".to_string());

            scm.push(format!(
                "(let loop ((i 0)) (if (< i {count}) (loop (+ i 1)) 'done))"
            ));
        }
        "if_header_equals" => {
            let header_name = block_value(block, "header_name", "x-debug");
            let equals_value = block_value(block, "equals_value", "on");
            let then_body = block_value(block, "then_body", "debug mode");
            let else_body = block_value(block, "else_body", "normal mode");

            rss.push(format!(
                "let __header_check = vm::http::request::get_header({});",
                rust_string(header_name)
            ));
            rss.push(format!(
                "if __header_check == {} {{",
                rust_string(equals_value)
            ));
            rss.push(format!(
                "    vm::http::response::set_body({});",
                rust_string(then_body)
            ));
            rss.push("} else {".to_string());
            rss.push(format!(
                "    vm::http::response::set_body({});",
                rust_string(else_body)
            ));
            rss.push("}".to_string());

            js.push(format!(
                "let __header_check = vm.http.request.get_header({});",
                js_string(header_name)
            ));
            js.push(format!(
                "if (__header_check === {}) {{",
                js_string(equals_value)
            ));
            js.push(format!(
                "    vm.http.response.set_body({});",
                js_string(then_body)
            ));
            js.push("} else {".to_string());
            js.push(format!(
                "    vm.http.response.set_body({});",
                js_string(else_body)
            ));
            js.push("}".to_string());

            lua.push(format!(
                "local __header_check = vm.http.request.get_header({})",
                lua_string(header_name)
            ));
            lua.push(format!(
                "if __header_check == {} then",
                lua_string(equals_value)
            ));
            lua.push(format!(
                "    vm.http.response.set_body({})",
                lua_string(then_body)
            ));
            lua.push("else".to_string());
            lua.push(format!(
                "    vm.http.response.set_body({})",
                lua_string(else_body)
            ));
            lua.push("end".to_string());

            scm.push(format!(
                "(let ((__header_check (vm.http.request.get_header {})))",
                scheme_string(header_name)
            ));
            scm.push(format!(
                "  (if (== __header_check {})",
                scheme_string(equals_value)
            ));
            scm.push(format!(
                "      (vm.http.response.set_body {})",
                scheme_string(then_body)
            ));
            scm.push(format!(
                "      (vm.http.response.set_body {})))",
                scheme_string(else_body)
            ));
        }
        "repeat_set_header" => {
            let count = sanitize_number(block.values.get("count"), "3");
            let header_name = block_value(block, "header_name", "x-loop");
            let header_value = block_value(block, "header_value", "on");

            rss.push(format!("for (let i = 0; i < {count}; i = i + 1) {{"));
            rss.push(format!(
                "    vm::http::response::set_header({}, {});",
                rust_string(header_name),
                rust_string(header_value)
            ));
            rss.push("}".to_string());

            js.push(format!("for (let i = 0; i < {count}; i = i + 1) {{"));
            js.push(format!(
                "    vm.http.response.set_header({}, {});",
                js_string(header_name),
                js_string(header_value)
            ));
            js.push("}".to_string());

            lua.push(format!("for i = 1, {count} do"));
            lua.push(format!(
                "    vm.http.response.set_header({}, {})",
                lua_string(header_name),
                lua_string(header_value)
            ));
            lua.push("end".to_string());

            scm.push("(let loop ((i 0))".to_string());
            scm.push(format!("  (if (< i {count})"));
            scm.push(format!(
                "      (begin (vm.http.response.set_header {} {}) (loop (+ i 1)))",
                scheme_string(header_name),
                scheme_string(header_value)
            ));
            scm.push("      'done))".to_string());
        }
        other => return Err(bad_request(&format!("unknown block_id '{other}'"))),
    }
    Ok(())
}

pub(super) fn parse_ui_flavor(
    value: Option<&str>,
) -> Result<(SourceFlavor, &'static str), (StatusCode, Json<ErrorResponse>)> {
    let raw = value.unwrap_or("rustscript").trim().to_ascii_lowercase();
    match raw.as_str() {
        "rustscript" | "rss" => Ok((SourceFlavor::RustScript, "rustscript")),
        "javascript" | "js" => Ok((SourceFlavor::JavaScript, "javascript")),
        "lua" => Ok((SourceFlavor::Lua, "lua")),
        "scheme" | "scm" => Ok((SourceFlavor::Scheme, "scheme")),
        _ => Err(bad_request(
            "flavor must be one of: rustscript, javascript, lua, scheme",
        )),
    }
}

pub(super) fn source_for_flavor(bundle: &UiSourceBundle, flavor: SourceFlavor) -> String {
    match flavor {
        SourceFlavor::RustScript => bundle.rustscript.clone(),
        SourceFlavor::JavaScript => bundle.javascript.clone(),
        SourceFlavor::Lua => bundle.lua.clone(),
        SourceFlavor::Scheme => bundle.scheme.clone(),
    }
}

fn join_lines(lines: &[String]) -> String {
    lines.join("\n")
}

fn sanitize_identifier(value: Option<&String>, fallback: &str) -> String {
    let raw = value.map(|v| v.trim()).unwrap_or("");
    let candidate = if raw.is_empty() { fallback } else { raw };
    let mut output = String::with_capacity(candidate.len());
    for (index, ch) in candidate.chars().enumerate() {
        let valid = ch == '_' || ch.is_ascii_alphanumeric();
        if !valid {
            continue;
        }
        if index == 0 && ch.is_ascii_digit() {
            output.push('_');
        }
        output.push(ch);
    }
    if output.is_empty() {
        fallback.to_string()
    } else {
        output
    }
}

fn sanitize_number(value: Option<&String>, fallback: &str) -> String {
    let raw = value.map(|v| v.trim()).unwrap_or("");
    if !raw.is_empty() && raw.chars().all(|ch| ch.is_ascii_digit()) {
        raw.to_string()
    } else {
        fallback.to_string()
    }
}

fn sanitize_status_code(value: Option<&String>, fallback: &str) -> String {
    let raw = sanitize_number(value, fallback);
    match raw.parse::<u16>() {
        Ok(code) if (100..=599).contains(&code) => code.to_string(),
        _ => fallback.to_string(),
    }
}

fn block_value<'a>(block: &'a UiBlockInstance, key: &str, fallback: &'a str) -> &'a str {
    block
        .values
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
}

fn render_expr_rss(raw: &str) -> String {
    render_expr_common(raw, rust_string)
}

fn render_expr_js(raw: &str) -> String {
    render_expr_common(raw, js_string)
}

fn render_expr_lua(raw: &str) -> String {
    render_expr_common(raw, lua_string)
}

fn render_expr_scheme(raw: &str) -> String {
    render_expr_common(raw, scheme_string)
}

fn render_expr_common(raw: &str, literal_renderer: fn(&str) -> String) -> String {
    if let Some(expr) = raw.strip_prefix('$') {
        let ident = sanitize_identifier(Some(&expr.to_string()), "value");
        return ident;
    }
    literal_renderer(raw)
}

fn rust_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

fn js_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

fn lua_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

fn scheme_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

fn escape_double_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\"', "\\\"")
        .replace('\n', "\\n")
}
