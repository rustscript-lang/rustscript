use super::graph::flow_action_statement;
use super::*;

pub(super) fn render_sources(
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

    ensure_host_namespace_imports(
        &mut rss_lines,
        &mut js_lines,
        &mut lua_lines,
        &mut scm_lines,
    );

    Ok(UiSourceBundle {
        rustscript: join_lines(&rss_lines),
        javascript: join_lines(&js_lines),
        lua: join_lines(&lua_lines),
        scheme: join_lines(&scm_lines),
    })
}

pub(super) fn ensure_host_namespace_imports(
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) {
    ensure_rustscript_host_namespace_imports(rss);
    ensure_javascript_host_namespace_imports(js);
    ensure_lua_host_namespace_imports(lua);
    ensure_scheme_host_namespace_imports(scm);
}

fn ensure_rustscript_host_namespace_imports(lines: &mut Vec<String>) {
    let requires_upstream_request = lines.iter().any(|line| line.contains("upstream_request::"));
    let requires_upstream_response = lines
        .iter()
        .any(|line| line.contains("upstream_response::"));
    let requires_io = lines.iter().any(|line| line.contains("io::"));
    let requires_re = lines.iter().any(|line| line.contains("re::"));
    let requires_json = lines.iter().any(|line| line.contains("json::"));
    let requires_runtime = lines.iter().any(|line| line.contains("runtime::"));
    let requires_rate_limit = lines.iter().any(|line| line.contains("rate_limit::"));

    if !requires_upstream_request
        && !requires_upstream_response
        && !requires_io
        && !requires_re
        && !requires_json
        && !requires_runtime
        && !requires_rate_limit
    {
        return;
    }

    let mut insert_at = 1usize;
    if requires_upstream_request
        && !lines
            .iter()
            .any(|line| line.trim() == "use edge::http::upstream::request as upstream_request;")
    {
        lines.insert(
            insert_at,
            "use edge::http::upstream::request as upstream_request;".to_string(),
        );
        insert_at += 1;
    }
    if requires_upstream_response
        && !lines
            .iter()
            .any(|line| line.trim() == "use edge::http::upstream::response as upstream_response;")
    {
        lines.insert(
            insert_at,
            "use edge::http::upstream::response as upstream_response;".to_string(),
        );
        insert_at += 1;
    }
    if requires_io && !lines.iter().any(|line| line.trim() == "use io;") {
        lines.insert(insert_at, "use io;".to_string());
        insert_at += 1;
    }
    if requires_re && !lines.iter().any(|line| line.trim() == "use re;") {
        lines.insert(insert_at, "use re;".to_string());
        insert_at += 1;
    }
    if requires_json && !lines.iter().any(|line| line.trim() == "use json;") {
        lines.insert(insert_at, "use json;".to_string());
        insert_at += 1;
    }
    if requires_runtime && !lines.iter().any(|line| line.trim() == "use runtime;") {
        lines.insert(insert_at, "use runtime;".to_string());
        insert_at += 1;
    }
    if requires_rate_limit && !lines.iter().any(|line| line.trim() == "use rate_limit;") {
        lines.insert(insert_at, "use rate_limit;".to_string());
        insert_at += 1;
    }

    if insert_at < lines.len() && !lines[insert_at].trim().is_empty() {
        lines.insert(insert_at, String::new());
    }
}

fn ensure_javascript_host_namespace_imports(lines: &mut Vec<String>) {
    let requires_upstream_request = lines.iter().any(|line| line.contains("upstream_request."));
    let requires_upstream_response = lines.iter().any(|line| line.contains("upstream_response."));
    let requires_io = lines.iter().any(|line| line.contains("io."));
    let requires_re = lines.iter().any(|line| line.contains("re."));
    let requires_json = lines.iter().any(|line| line.contains("json."));
    if !requires_upstream_request
        && !requires_upstream_response
        && !requires_io
        && !requires_re
        && !requires_json
    {
        return;
    }

    let mut insert_at = 1usize;
    if requires_upstream_request
        && !lines.iter().any(|line| {
            line.trim() == "import * as upstream_request from \"edge/http/upstream/request.rss\";"
        })
    {
        lines.insert(
            insert_at,
            "import * as upstream_request from \"edge/http/upstream/request.rss\";".to_string(),
        );
        insert_at += 1;
    }
    if requires_upstream_response
        && !lines.iter().any(|line| {
            line.trim() == "import * as upstream_response from \"edge/http/upstream/response.rss\";"
        })
    {
        lines.insert(
            insert_at,
            "import * as upstream_response from \"edge/http/upstream/response.rss\";".to_string(),
        );
        insert_at += 1;
    }
    if requires_io
        && !lines
            .iter()
            .any(|line| line.trim() == "import * as io from \"io\";")
    {
        lines.insert(insert_at, "import * as io from \"io\";".to_string());
        insert_at += 1;
    }
    if requires_re
        && !lines
            .iter()
            .any(|line| line.trim() == "import * as re from \"re\";")
    {
        lines.insert(insert_at, "import * as re from \"re\";".to_string());
        insert_at += 1;
    }
    if requires_json
        && !lines
            .iter()
            .any(|line| line.trim() == "import * as json from \"json\";")
    {
        lines.insert(insert_at, "import * as json from \"json\";".to_string());
        insert_at += 1;
    }

    if insert_at < lines.len() && !lines[insert_at].trim().is_empty() {
        lines.insert(insert_at, String::new());
    }
}

fn ensure_lua_host_namespace_imports(lines: &mut Vec<String>) {
    let requires_upstream_request = lines.iter().any(|line| line.contains("upstream_request."));
    let requires_upstream_response = lines.iter().any(|line| line.contains("upstream_response."));
    let requires_io = lines.iter().any(|line| line.contains("io."));
    let requires_re = lines.iter().any(|line| line.contains("re."));
    let requires_json = lines.iter().any(|line| line.contains("json."));
    if !requires_upstream_request
        && !requires_upstream_response
        && !requires_io
        && !requires_re
        && !requires_json
    {
        return;
    }

    let mut insert_at = 1usize;
    if requires_upstream_request
        && !lines.iter().any(|line| {
            line.trim() == "local upstream_request = require(\"edge/http/upstream/request.rss\")"
        })
    {
        lines.insert(
            insert_at,
            "local upstream_request = require(\"edge/http/upstream/request.rss\")".to_string(),
        );
        insert_at += 1;
    }
    if requires_upstream_response
        && !lines.iter().any(|line| {
            line.trim() == "local upstream_response = require(\"edge/http/upstream/response.rss\")"
        })
    {
        lines.insert(
            insert_at,
            "local upstream_response = require(\"edge/http/upstream/response.rss\")".to_string(),
        );
        insert_at += 1;
    }
    if requires_io
        && !lines
            .iter()
            .any(|line| line.trim() == "local io = require(\"io\")")
    {
        lines.insert(insert_at, "local io = require(\"io\")".to_string());
        insert_at += 1;
    }
    if requires_re
        && !lines
            .iter()
            .any(|line| line.trim() == "local re = require(\"re\")")
    {
        lines.insert(insert_at, "local re = require(\"re\")".to_string());
        insert_at += 1;
    }
    if requires_json
        && !lines
            .iter()
            .any(|line| line.trim() == "local json = require(\"json\")")
    {
        lines.insert(insert_at, "local json = require(\"json\")".to_string());
        insert_at += 1;
    }

    if insert_at < lines.len() && !lines[insert_at].trim().is_empty() {
        lines.insert(insert_at, String::new());
    }
}

fn ensure_scheme_host_namespace_imports(lines: &mut Vec<String>) {
    let requires_upstream_request = lines.iter().any(|line| line.contains("(upstream_request:"));
    let requires_upstream_response = lines
        .iter()
        .any(|line| line.contains("(upstream_response:"));
    let requires_io = lines.iter().any(|line| line.contains("(io."));
    let requires_re = lines.iter().any(|line| line.contains("(re."));
    let requires_json = lines.iter().any(|line| line.contains("(json."));
    if !requires_upstream_request
        && !requires_upstream_response
        && !requires_io
        && !requires_re
        && !requires_json
    {
        return;
    }

    let mut insert_at = 1usize;
    if requires_upstream_request
        && !lines.iter().any(|line| {
            line.trim() == "(import (prefix \"edge/http/upstream/request.rss\" upstream_request:))"
        })
    {
        lines.insert(
            insert_at,
            "(import (prefix \"edge/http/upstream/request.rss\" upstream_request:))".to_string(),
        );
        insert_at += 1;
    }
    if requires_upstream_response
        && !lines.iter().any(|line| {
            line.trim()
                == "(import (prefix \"edge/http/upstream/response.rss\" upstream_response:))"
        })
    {
        lines.insert(
            insert_at,
            "(import (prefix \"edge/http/upstream/response.rss\" upstream_response:))".to_string(),
        );
        insert_at += 1;
    }
    if requires_io
        && !lines
            .iter()
            .any(|line| line.trim() == "(require (prefix-in io. \"io\"))")
    {
        lines.insert(insert_at, "(require (prefix-in io. \"io\"))".to_string());
        insert_at += 1;
    }
    if requires_re
        && !lines
            .iter()
            .any(|line| line.trim() == "(require (prefix-in re. \"re\"))")
    {
        lines.insert(insert_at, "(require (prefix-in re. \"re\"))".to_string());
        insert_at += 1;
    }
    if requires_json
        && !lines
            .iter()
            .any(|line| line.trim() == "(require (prefix-in json. \"json\"))")
    {
        lines.insert(
            insert_at,
            "(require (prefix-in json. \"json\"))".to_string(),
        );
        insert_at += 1;
    }

    if insert_at < lines.len() && !lines[insert_at].trim().is_empty() {
        lines.insert(insert_at, String::new());
    }
}

pub(super) fn render_single_block(
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
        "get_request_body_next_chunk" => {
            let var = sanitize_identifier(block.values.get("var"), "request_chunk");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "1024"), "1024");
            rss.push(format!(
                "let {var} = vm::http::request::body::next_chunk({max_bytes});"
            ));
            js.push(format!(
                "let {var} = vm.http.request.body.next_chunk({max_bytes});"
            ));
            lua.push(format!(
                "local {var} = vm.http.request.body.next_chunk({max_bytes})"
            ));
            scm.push(format!(
                "(define {var} (vm.http.request.body.next_chunk {max_bytes}))"
            ));
        }
        "get_request_body_eof" => {
            let var = sanitize_identifier(block.values.get("var"), "request_body_eof");
            rss.push(format!("let {var} = vm::http::request::body::eof();"));
            js.push(format!("let {var} = vm.http.request.body.eof();"));
            lua.push(format!("local {var} = vm.http.request.body.eof()"));
            scm.push(format!("(define {var} (vm.http.request.body.eof))"));
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
            rss.push(format!("let {var} = upstream_response::get_status();"));
            js.push(format!("let {var} = upstream_response.get_status();"));
            lua.push(format!("local {var} = upstream_response.get_status()"));
            scm.push(format!("(define {var} (upstream_response:get_status))"));
        }
        "get_upstream_response_header" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_header");
            let header_name = block_value(block, "name", "x-upstream");
            rss.push(format!(
                "let {var} = upstream_response::get_header({});",
                rust_string(header_name)
            ));
            js.push(format!(
                "let {var} = upstream_response.get_header({});",
                js_string(header_name)
            ));
            lua.push(format!(
                "local {var} = upstream_response.get_header({})",
                lua_string(header_name)
            ));
            scm.push(format!(
                "(define {var} (upstream_response:get_header {}))",
                scheme_string(header_name)
            ));
        }
        "get_upstream_response_headers" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_headers");
            rss.push(format!("let {var} = upstream_response::get_headers();"));
            js.push(format!("let {var} = upstream_response.get_headers();"));
            lua.push(format!("local {var} = upstream_response.get_headers()"));
            scm.push(format!("(define {var} (upstream_response:get_headers))"));
        }
        "get_upstream_response_body" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_body");
            rss.push(format!("let {var} = upstream_response::get_body();"));
            js.push(format!("let {var} = upstream_response.get_body();"));
            lua.push(format!("local {var} = upstream_response.get_body()"));
            scm.push(format!("(define {var} (upstream_response:get_body))"));
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
            rss.push(format!("let {var} = ({}).length;", render_expr_rss(value)));
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
            let rss_expr = render_slice_expr(render_expr_rss(value), start.clone(), end.clone());
            let js_expr = render_slice_expr(render_expr_js(value), start.clone(), end.clone());
            let lua_expr = render_slice_expr(render_expr_lua(value), start.clone(), end.clone());

            rss.push(format!("let {var} = {rss_expr};"));
            js.push(format!("let {var} = {js_expr};"));
            lua.push(format!("local {var} = {lua_expr}"));

            let scheme_value = render_expr_scheme(value);
            let scheme_expr = match (start, end) {
                (Some(start), Some(end)) => {
                    format!("(slice-range {scheme_value} {start} {end})")
                }
                (None, Some(end)) => format!("(slice-to {scheme_value} {end})"),
                (Some(start), None) => format!("(slice-from {scheme_value} {start})"),
                (None, None) => format!("(slice-from {scheme_value} 0)"),
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
            rss.push(format!(
                "let mut {var} = {};",
                render_detached_expr_rss(array)
            ));
            rss.push(format!("{var}[{var}.length] = {};", render_expr_rss(value)));
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
                "let {var} = (({})[{index}]).copy();",
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
            rss.push(format!(
                "let mut {var} = {};",
                render_detached_expr_rss(array)
            ));
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
            let rss_access = render_key_access_expr(render_expr_rss(map), key, render_expr_rss);
            rss.push(format!("let {var} = ({rss_access}).copy();"));
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
            rss.push(format!(
                "let mut {var} = {};",
                render_detached_expr_rss(map)
            ));
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
        "json_encode" => {
            let var = sanitize_identifier(block.values.get("var"), "json_text");
            let value = block_value(block, "value", "$payload");
            rss.push(format!(
                "let {var} = json::encode({});",
                render_expr_rss(value)
            ));
            js.push(format!(
                "let {var} = json.encode({});",
                render_expr_js(value)
            ));
            lua.push(format!(
                "local {var} = json.encode({})",
                render_expr_lua(value)
            ));
            scm.push(format!(
                "(define {var} (json.encode {}))",
                render_expr_scheme(value)
            ));
        }
        "json_decode" => {
            let var = sanitize_identifier(block.values.get("var"), "json_value");
            let value = block_value(block, "value", "$json_text");
            rss.push(format!(
                "let {var} = json::decode({});",
                render_expr_rss(value)
            ));
            js.push(format!(
                "let {var} = json.decode({});",
                render_expr_js(value)
            ));
            lua.push(format!(
                "local {var} = json.decode({})",
                render_expr_lua(value)
            ));
            scm.push(format!(
                "(define {var} (json.decode {}))",
                render_expr_scheme(value)
            ));
        }
        "set_request_header"
        | "add_request_header"
        | "clear_request_header"
        | "set_request_method"
        | "set_request_path"
        | "set_request_query"
        | "set_request_query_arg"
        | "set_request_body"
        | "set_header"
        | "add_response_header"
        | "clear_response_header"
        | "set_response_content"
        | "set_response_status"
        | "set_upstream"
        | "runtime_sleep" => {
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
                "let {var} = rate_limit::allow({}, {}, {});",
                render_expr_rss(key_expr),
                limit,
                window
            ));
            js.push(format!(
                "let {var} = vm.rate_limit.allow({}, {}, {});",
                render_expr_js(key_expr),
                limit,
                window
            ));
            lua.push(format!(
                "local {var} = vm.rate_limit.allow({}, {}, {})",
                render_expr_lua(key_expr),
                limit,
                window
            ));
            scm.push(format!(
                "(define {var} (vm.rate_limit.allow {} {} {}))",
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
                "if rate_limit::allow({}, {}, {}) {{",
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
                "if (vm.rate_limit.allow({}, {}, {})) {{",
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
                "if vm.rate_limit.allow({}, {}, {}) then",
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
                "(if (vm.rate_limit.allow {} {} {})",
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

            rss.push(format!("for (let mut i = 0; i < {count}; i = i + 1) {{"));
            rss.push("}".to_string());

            js.push(format!("for (let i = 0; i < {count}; i = i + 1) {{"));
            js.push("}".to_string());

            lua.push(format!("for i = 1, {count}, 1 do"));
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

            rss.push(format!("for (let mut i = 0; i < {count}; i = i + 1) {{"));
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

            lua.push(format!("for i = 1, {count}, 1 do"));
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

pub(super) fn scheme_branch_expr(expressions: &[String]) -> String {
    if expressions.is_empty() {
        return "null".to_string();
    }
    if expressions.len() == 1 {
        return expressions[0].clone();
    }
    format!("(begin {})", expressions.join(" "))
}

pub(super) fn render_number_expr(raw: &str, fallback: &str) -> String {
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

pub(super) fn render_slice_expr(
    value_expr: String,
    start: Option<String>,
    end: Option<String>,
) -> String {
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

pub(super) fn render_key_access_expr(
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

pub(super) fn render_key_assignment_target(
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

pub(super) fn render_scheme_hash_key_expr(raw_key: &str) -> String {
    if let Some(member) = map_key_member_name(raw_key) {
        member.to_string()
    } else {
        render_expr_scheme(raw_key)
    }
}

pub(super) fn join_lines(lines: &[String]) -> String {
    lines.join("\n")
}

pub(super) fn sanitize_identifier(value: Option<&String>, fallback: &str) -> String {
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

pub(super) fn sanitize_number(value: Option<&String>, fallback: &str) -> String {
    let raw = value.map(|v| v.trim()).unwrap_or("");
    if !raw.is_empty() && raw.chars().all(|ch| ch.is_ascii_digit()) {
        raw.to_string()
    } else {
        fallback.to_string()
    }
}

pub(super) fn sanitize_status_code(value: Option<&String>, fallback: &str) -> String {
    let raw = sanitize_number(value, fallback);
    match raw.parse::<u16>() {
        Ok(code) if (100..=599).contains(&code) => code.to_string(),
        _ => fallback.to_string(),
    }
}

pub(super) fn block_value<'a>(block: &'a UiBlockInstance, key: &str, fallback: &'a str) -> &'a str {
    block
        .values
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
}

pub(super) fn render_expr_rss(raw: &str) -> String {
    render_expr_common(raw, rust_string)
}

pub(super) fn render_detached_expr_rss(raw: &str) -> String {
    format!("({}).copy()", render_expr_rss(raw))
}

pub(super) fn render_expr_js(raw: &str) -> String {
    render_expr_common(raw, js_string)
}

pub(super) fn render_expr_lua(raw: &str) -> String {
    render_expr_common(raw, lua_string)
}

pub(super) fn render_expr_scheme(raw: &str) -> String {
    render_expr_common(raw, scheme_string)
}

fn render_expr_common(raw: &str, literal_renderer: fn(&str) -> String) -> String {
    if let Some(expr) = raw.strip_prefix('$') {
        let ident = sanitize_identifier(Some(&expr.to_string()), "value");
        return ident;
    }
    literal_renderer(raw)
}

pub(super) fn rust_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

pub(super) fn js_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

pub(super) fn lua_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

pub(super) fn scheme_string(value: &str) -> String {
    format!("\"{}\"", escape_double_quoted(value))
}

fn escape_double_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\"', "\\\"")
        .replace('\n', "\\n")
}
