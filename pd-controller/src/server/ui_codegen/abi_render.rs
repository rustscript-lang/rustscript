use super::render::*;
use super::*;

pub(super) fn is_additional_pure_value_block(block_id: &str) -> bool {
    matches!(
        block_id,
        "http_exchange_new"
            | "http_exchange_default_upstream"
            | "http_upstream_as_stream"
            | "get_upstream_response_http_version"
            | "get_upstream_response_next_chunk"
            | "get_upstream_response_eof"
            | "read_upstream_response_line"
            | "read_upstream_response_all"
            | "tcp_stream_downstream"
            | "tcp_stream_default_upstream"
            | "tcp_stream_new"
            | "tls_session_from_socket"
            | "websocket_connection_new"
            | "websocket_connection_downstream"
            | "websocket_connection_default_upstream"
            | "webrtc_connection_new"
            | "webrtc_connection_downstream"
            | "webrtc_connection_default_upstream"
            | "udp_socket_new"
            | "udp_socket_downstream"
            | "udp_socket_default_upstream"
            | "proxy_stream_downstream"
            | "proxy_stream_exchange"
            | "proxy_stream_from_tcp"
            | "proxy_stream_from_tls_plaintext"
            | "proxy_stream_from_websocket_binary"
    )
}

pub(super) fn is_additional_mixed_flow_block(block_id: &str) -> bool {
    matches!(
        block_id,
        "http_exchange_get_status"
            | "http_exchange_get_header"
            | "http_exchange_get_headers"
            | "http_exchange_get_body"
            | "http_exchange_get_http_version"
            | "http_exchange_body_next_chunk"
            | "http_exchange_body_eof"
            | "tcp_stream_is_present"
            | "tcp_stream_connect"
            | "tcp_stream_get_phase"
            | "tcp_stream_get_local_addr"
            | "tcp_stream_get_peer_addr"
            | "tcp_stream_read"
            | "tcp_stream_peek"
            | "tcp_stream_write"
            | "tcp_stream_eof"
            | "tls_session_is_present"
            | "tls_session_handshake"
            | "tls_session_get_peer_name"
            | "tls_session_get_alpn"
            | "tls_session_get_phase"
            | "tls_session_get_peer_certificate"
            | "tls_session_is_session_reused"
            | "websocket_connection_is_present"
            | "websocket_connection_connect"
            | "websocket_connection_get_phase"
            | "websocket_connection_get_subprotocol"
            | "websocket_connection_send_text"
            | "websocket_connection_read_text"
            | "websocket_connection_send_binary_base64"
            | "websocket_connection_read_binary_base64"
            | "websocket_connection_eof"
            | "webrtc_connection_is_present"
            | "webrtc_connection_create_offer"
            | "webrtc_connection_create_answer"
            | "webrtc_connection_connect"
            | "webrtc_connection_get_phase"
            | "webrtc_connection_send_text"
            | "webrtc_connection_read_text"
            | "webrtc_connection_send_binary_base64"
            | "webrtc_connection_read_binary_base64"
            | "webrtc_connection_eof"
            | "udp_socket_is_present"
            | "udp_socket_connect"
            | "udp_socket_get_phase"
            | "udp_socket_get_local_addr"
            | "udp_socket_get_peer_addr"
            | "udp_socket_send_text"
            | "udp_socket_recv_text"
            | "udp_socket_send_binary_base64"
            | "udp_socket_recv_binary_base64"
            | "proxy_pipe"
            | "proxy_bridge"
    )
}

pub(super) fn is_additional_flow_block(block_id: &str) -> bool {
    is_additional_mixed_flow_block(block_id)
        || matches!(
            block_id,
            "http_downstream_attach_transport"
                | "http_exchange_send"
                | "http_exchange_set_header"
                | "http_exchange_add_header"
                | "http_exchange_clear_header"
                | "http_exchange_set_method"
                | "http_exchange_set_path"
                | "http_exchange_set_query"
                | "http_exchange_set_target"
                | "http_exchange_attach_tcp"
                | "http_exchange_attach_tls_plaintext"
                | "http_exchange_set_body"
                | "http_exchange_set_query_arg"
                | "tcp_stream_bind"
                | "tcp_stream_set_target"
                | "tcp_stream_close"
                | "tls_session_set_alpn"
                | "tls_session_set_verify"
                | "tls_session_set_verify_hostname"
                | "tls_session_set_trusted_certificate"
                | "tls_session_set_client_certificate"
                | "tls_session_set_client_private_key"
                | "tls_session_set_server_certificate"
                | "tls_session_set_server_private_key"
                | "tls_session_set_sni"
                | "tls_session_set_min_version"
                | "tls_session_set_max_version"
                | "websocket_connection_set_target"
                | "websocket_connection_set_header"
                | "websocket_connection_set_subprotocols"
                | "websocket_connection_close"
                | "webrtc_connection_set_ice_servers"
                | "webrtc_connection_set_data_channel_label"
                | "webrtc_connection_set_remote_description"
                | "webrtc_connection_close"
                | "udp_socket_bind"
                | "udp_socket_set_target"
                | "udp_socket_close"
        )
}

pub(super) fn render_additional_value_block(
    block: &UiBlockInstance,
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    if render_additional_http_value_block(block, rss, js, lua, scm)?
        || render_additional_transport_value_block(block, rss, js, lua, scm)?
        || render_additional_realtime_value_block(block, rss, js, lua, scm)?
        || render_additional_proxy_value_block(block, rss, js, lua, scm)?
    {
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn additional_flow_action_statement(
    block: &UiBlockInstance,
) -> Option<Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)>> {
    additional_http_flow_action(block)
        .or_else(|| additional_transport_flow_action(block))
        .or_else(|| additional_realtime_flow_action(block))
        .or_else(|| additional_proxy_flow_action(block))
}

fn render_additional_http_value_block(
    block: &UiBlockInstance,
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    match block.block_id.as_str() {
        "http_exchange_new" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::http::exchange::new()".to_string(),
                "vm.http.exchange.new()".to_string(),
                "vm.http.exchange.new()".to_string(),
                "(vm.http.exchange.new)".to_string(),
            );
            Ok(true)
        }
        "http_exchange_default_upstream" => {
            let var = sanitize_identifier(block.values.get("var"), "default_exchange");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::http::exchange::default_upstream()".to_string(),
                "vm.http.exchange.default_upstream()".to_string(),
                "vm.http.exchange.default_upstream()".to_string(),
                "(vm.http.exchange.default_upstream)".to_string(),
            );
            Ok(true)
        }
        "http_upstream_as_stream" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_stream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "upstream::as_stream()".to_string(),
                "upstream.as_stream()".to_string(),
                "upstream.as_stream()".to_string(),
                "(upstream:as_stream)".to_string(),
            );
            Ok(true)
        }
        "get_upstream_response_http_version" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_http_version");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "upstream_response::get_http_version()".to_string(),
                "upstream_response.get_http_version()".to_string(),
                "upstream_response.get_http_version()".to_string(),
                "(upstream_response:get_http_version)".to_string(),
            );
            Ok(true)
        }
        "get_upstream_response_next_chunk" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_chunk");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "1024"), "1024");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                format!("upstream_response::next_chunk({max_bytes})"),
                format!("upstream_response.next_chunk({max_bytes})"),
                format!("upstream_response.next_chunk({max_bytes})"),
                format!("(upstream_response:next_chunk {max_bytes})"),
            );
            Ok(true)
        }
        "get_upstream_response_eof" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_body_eof");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "upstream_response::eof()".to_string(),
                "upstream_response.eof()".to_string(),
                "upstream_response.eof()".to_string(),
                "(upstream_response:eof)".to_string(),
            );
            Ok(true)
        }
        "read_upstream_response_line" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_line");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "upstream_response::read_line()".to_string(),
                "upstream_response.read_line()".to_string(),
                "upstream_response.read_line()".to_string(),
                "(upstream_response:read_line)".to_string(),
            );
            Ok(true)
        }
        "read_upstream_response_all" => {
            let var = sanitize_identifier(block.values.get("var"), "upstream_all");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "upstream_response::read_all()".to_string(),
                "upstream_response.read_all()".to_string(),
                "upstream_response.read_all()".to_string(),
                "(upstream_response:read_all)".to_string(),
            );
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn render_additional_transport_value_block(
    block: &UiBlockInstance,
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    match block.block_id.as_str() {
        "tcp_stream_downstream" => {
            let var = sanitize_identifier(block.values.get("var"), "downstream_stream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::tcp::stream::downstream()".to_string(),
                "vm.tcp.stream.downstream()".to_string(),
                "vm.tcp.stream.downstream()".to_string(),
                "(vm.tcp.stream.downstream)".to_string(),
            );
            Ok(true)
        }
        "tcp_stream_default_upstream" => {
            let var = sanitize_identifier(block.values.get("var"), "default_tcp_upstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::tcp::stream::default_upstream()".to_string(),
                "vm.tcp.stream.default_upstream()".to_string(),
                "vm.tcp.stream.default_upstream()".to_string(),
                "(vm.tcp.stream.default_upstream)".to_string(),
            );
            Ok(true)
        }
        "tcp_stream_new" => {
            let var = sanitize_identifier(block.values.get("var"), "tcp_stream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::tcp::stream::new()".to_string(),
                "vm.tcp.stream.new()".to_string(),
                "vm.tcp.stream.new()".to_string(),
                "(vm.tcp.stream.new)".to_string(),
            );
            Ok(true)
        }
        "tls_session_from_socket" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_session");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                format!("vm::tls::session::from_socket({stream})"),
                format!("vm.tls.session.from_socket({stream})"),
                format!("vm.tls.session.from_socket({stream})"),
                format!("(vm.tls.session.from_socket {stream})"),
            );
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn render_additional_realtime_value_block(
    block: &UiBlockInstance,
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    let handled = match block.block_id.as_str() {
        "websocket_connection_new" => {
            let var = sanitize_identifier(block.values.get("var"), "ws_conn");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::websocket::connection::new()".to_string(),
                "vm.websocket.connection.new()".to_string(),
                "vm.websocket.connection.new()".to_string(),
                "(vm.websocket.connection.new)".to_string(),
            );
            true
        }
        "websocket_connection_downstream" => {
            let var = sanitize_identifier(block.values.get("var"), "ws_downstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::websocket::connection::downstream()".to_string(),
                "vm.websocket.connection.downstream()".to_string(),
                "vm.websocket.connection.downstream()".to_string(),
                "(vm.websocket.connection.downstream)".to_string(),
            );
            true
        }
        "websocket_connection_default_upstream" => {
            let var = sanitize_identifier(block.values.get("var"), "ws_default_upstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::websocket::connection::default_upstream()".to_string(),
                "vm.websocket.connection.default_upstream()".to_string(),
                "vm.websocket.connection.default_upstream()".to_string(),
                "(vm.websocket.connection.default_upstream)".to_string(),
            );
            true
        }
        "webrtc_connection_new" => {
            let var = sanitize_identifier(block.values.get("var"), "webrtc_conn");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::webrtc::connection::new()".to_string(),
                "vm.webrtc.connection.new()".to_string(),
                "vm.webrtc.connection.new()".to_string(),
                "(vm.webrtc.connection.new)".to_string(),
            );
            true
        }
        "webrtc_connection_downstream" => {
            let var = sanitize_identifier(block.values.get("var"), "webrtc_downstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::webrtc::connection::downstream()".to_string(),
                "vm.webrtc.connection.downstream()".to_string(),
                "vm.webrtc.connection.downstream()".to_string(),
                "(vm.webrtc.connection.downstream)".to_string(),
            );
            true
        }
        "webrtc_connection_default_upstream" => {
            let var = sanitize_identifier(block.values.get("var"), "webrtc_default_upstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::webrtc::connection::default_upstream()".to_string(),
                "vm.webrtc.connection.default_upstream()".to_string(),
                "vm.webrtc.connection.default_upstream()".to_string(),
                "(vm.webrtc.connection.default_upstream)".to_string(),
            );
            true
        }
        "udp_socket_new" => {
            let var = sanitize_identifier(block.values.get("var"), "udp_socket");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::udp::socket::new()".to_string(),
                "vm.udp.socket.new()".to_string(),
                "vm.udp.socket.new()".to_string(),
                "(vm.udp.socket.new)".to_string(),
            );
            true
        }
        "udp_socket_downstream" => {
            let var = sanitize_identifier(block.values.get("var"), "udp_downstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::udp::socket::downstream()".to_string(),
                "vm.udp.socket.downstream()".to_string(),
                "vm.udp.socket.downstream()".to_string(),
                "(vm.udp.socket.downstream)".to_string(),
            );
            true
        }
        "udp_socket_default_upstream" => {
            let var = sanitize_identifier(block.values.get("var"), "udp_default_upstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::udp::socket::default_upstream()".to_string(),
                "vm.udp.socket.default_upstream()".to_string(),
                "vm.udp.socket.default_upstream()".to_string(),
                "(vm.udp.socket.default_upstream)".to_string(),
            );
            true
        }
        _ => false,
    };
    Ok(handled)
}

fn render_additional_proxy_value_block(
    block: &UiBlockInstance,
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    let handled = match block.block_id.as_str() {
        "proxy_stream_downstream" => {
            let var = sanitize_identifier(block.values.get("var"), "proxy_downstream");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                "vm::proxy::stream::downstream()".to_string(),
                "vm.proxy.stream.downstream()".to_string(),
                "vm.proxy.stream.downstream()".to_string(),
                "(vm.proxy.stream.downstream)".to_string(),
            );
            true
        }
        "proxy_stream_exchange" => {
            let var = sanitize_identifier(block.values.get("var"), "proxy_exchange_stream");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                format!("vm::proxy::stream::exchange({exchange})"),
                format!("vm.proxy.stream.exchange({exchange})"),
                format!("vm.proxy.stream.exchange({exchange})"),
                format!("(vm.proxy.stream.exchange {exchange})"),
            );
            true
        }
        "proxy_stream_from_tcp" => {
            let var = sanitize_identifier(block.values.get("var"), "proxy_tcp_stream");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                format!("vm::proxy::stream::from_tcp({stream})"),
                format!("vm.proxy.stream.from_tcp({stream})"),
                format!("vm.proxy.stream.from_tcp({stream})"),
                format!("(vm.proxy.stream.from_tcp {stream})"),
            );
            true
        }
        "proxy_stream_from_tls_plaintext" => {
            let var = sanitize_identifier(block.values.get("var"), "proxy_tls_stream");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                format!("vm::proxy::stream::from_tls_plaintext({session})"),
                format!("vm.proxy.stream.from_tls_plaintext({session})"),
                format!("vm.proxy.stream.from_tls_plaintext({session})"),
                format!("(vm.proxy.stream.from_tls_plaintext {session})"),
            );
            true
        }
        "proxy_stream_from_websocket_binary" => {
            let var = sanitize_identifier(block.values.get("var"), "proxy_ws_stream");
            let connection = render_number_expr(block_value(block, "connection", "1"), "1");
            push_value_assignment(
                rss,
                js,
                lua,
                scm,
                &var,
                format!("vm::proxy::stream::from_websocket_binary({connection})"),
                format!("vm.proxy.stream.from_websocket_binary({connection})"),
                format!("vm.proxy.stream.from_websocket_binary({connection})"),
                format!("(vm.proxy.stream.from_websocket_binary {connection})"),
            );
            true
        }
        _ => false,
    };
    Ok(handled)
}

fn additional_http_flow_action(
    block: &UiBlockInstance,
) -> Option<Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)>> {
    match block.block_id.as_str() {
        "http_downstream_attach_transport" => Some(Ok(statement_action(
            "vm::http::downstream::attach_transport();".to_string(),
            "vm.http.downstream.attach_transport();".to_string(),
            "vm.http.downstream.attach_transport()".to_string(),
            "(vm.http.downstream.attach_transport)".to_string(),
        ))),
        "http_exchange_send" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            Some(Ok(statement_action(
                format!("vm::http::exchange::send({exchange});"),
                format!("vm.http.exchange.send({exchange});"),
                format!("vm.http.exchange.send({exchange})"),
                format!("(vm.http.exchange.send {exchange})"),
            )))
        }
        "http_exchange_set_header" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let name = block_value(block, "name", "x-added");
            let value = block_value(block, "value", "1");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_header({exchange}, {}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                format!(
                    "vm.http.exchange.set_header({exchange}, {}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                format!(
                    "vm.http.exchange.set_header({exchange}, {}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                format!(
                    "(vm.http.exchange.set_header {exchange} {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            )))
        }
        "http_exchange_add_header" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let name = block_value(block, "name", "set-cookie");
            let value = block_value(block, "value", "a=1");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::add_header({exchange}, {}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                format!(
                    "vm.http.exchange.add_header({exchange}, {}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                format!(
                    "vm.http.exchange.add_header({exchange}, {}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                format!(
                    "(vm.http.exchange.add_header {exchange} {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            )))
        }
        "http_exchange_clear_header" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let name = block_value(block, "name", "x-remove");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::clear_header({exchange}, {});",
                    rust_string(name)
                ),
                format!(
                    "vm.http.exchange.clear_header({exchange}, {});",
                    js_string(name)
                ),
                format!(
                    "vm.http.exchange.clear_header({exchange}, {})",
                    lua_string(name)
                ),
                format!(
                    "(vm.http.exchange.clear_header {exchange} {})",
                    scheme_string(name)
                ),
            )))
        }
        "http_exchange_set_method" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let method = block_value(block, "method", "GET");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_method({exchange}, {});",
                    render_expr_rss(method)
                ),
                format!(
                    "vm.http.exchange.set_method({exchange}, {});",
                    render_expr_js(method)
                ),
                format!(
                    "vm.http.exchange.set_method({exchange}, {})",
                    render_expr_lua(method)
                ),
                format!(
                    "(vm.http.exchange.set_method {exchange} {})",
                    render_expr_scheme(method)
                ),
            )))
        }
        "http_exchange_set_path" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let path = block_value(block, "path", "/");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_path({exchange}, {});",
                    render_expr_rss(path)
                ),
                format!(
                    "vm.http.exchange.set_path({exchange}, {});",
                    render_expr_js(path)
                ),
                format!(
                    "vm.http.exchange.set_path({exchange}, {})",
                    render_expr_lua(path)
                ),
                format!(
                    "(vm.http.exchange.set_path {exchange} {})",
                    render_expr_scheme(path)
                ),
            )))
        }
        "http_exchange_set_query" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let query = block_value(block, "query", "x=1");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_query({exchange}, {});",
                    render_expr_rss(query)
                ),
                format!(
                    "vm.http.exchange.set_query({exchange}, {});",
                    render_expr_js(query)
                ),
                format!(
                    "vm.http.exchange.set_query({exchange}, {})",
                    render_expr_lua(query)
                ),
                format!(
                    "(vm.http.exchange.set_query {exchange} {})",
                    render_expr_scheme(query)
                ),
            )))
        }
        "http_exchange_set_target" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let target = block_value(block, "target", "127.0.0.1:8080");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_target({exchange}, {});",
                    render_expr_rss(target)
                ),
                format!(
                    "vm.http.exchange.set_target({exchange}, {});",
                    render_expr_js(target)
                ),
                format!(
                    "vm.http.exchange.set_target({exchange}, {})",
                    render_expr_lua(target)
                ),
                format!(
                    "(vm.http.exchange.set_target {exchange} {})",
                    render_expr_scheme(target)
                ),
            )))
        }
        "http_exchange_attach_tcp" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(statement_action(
                format!("vm::http::exchange::attach_tcp({exchange}, {stream});"),
                format!("vm.http.exchange.attach_tcp({exchange}, {stream});"),
                format!("vm.http.exchange.attach_tcp({exchange}, {stream})"),
                format!("(vm.http.exchange.attach_tcp {exchange} {stream})"),
            )))
        }
        "http_exchange_attach_tls_plaintext" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(statement_action(
                format!("vm::http::exchange::attach_tls_plaintext({exchange}, {session});"),
                format!("vm.http.exchange.attach_tls_plaintext({exchange}, {session});"),
                format!("vm.http.exchange.attach_tls_plaintext({exchange}, {session})"),
                format!("(vm.http.exchange.attach_tls_plaintext {exchange} {session})"),
            )))
        }
        "http_exchange_set_body" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let value = block_value(block, "value", "payload");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_body({exchange}, {});",
                    render_expr_rss(value)
                ),
                format!(
                    "vm.http.exchange.set_body({exchange}, {});",
                    render_expr_js(value)
                ),
                format!(
                    "vm.http.exchange.set_body({exchange}, {})",
                    render_expr_lua(value)
                ),
                format!(
                    "(vm.http.exchange.set_body {exchange} {})",
                    render_expr_scheme(value)
                ),
            )))
        }
        "http_exchange_set_query_arg" => {
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let name = block_value(block, "name", "id");
            let value = block_value(block, "value", "1");
            Some(Ok(statement_action(
                format!(
                    "vm::http::exchange::set_query_arg({exchange}, {}, {});",
                    rust_string(name),
                    render_expr_rss(value)
                ),
                format!(
                    "vm.http.exchange.set_query_arg({exchange}, {}, {});",
                    js_string(name),
                    render_expr_js(value)
                ),
                format!(
                    "vm.http.exchange.set_query_arg({exchange}, {}, {})",
                    lua_string(name),
                    render_expr_lua(value)
                ),
                format!(
                    "(vm.http.exchange.set_query_arg {exchange} {} {})",
                    scheme_string(name),
                    render_expr_scheme(value)
                ),
            )))
        }
        "http_exchange_get_status" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_status");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::http::exchange::get_status({exchange})"),
                format!("vm.http.exchange.get_status({exchange})"),
                format!("vm.http.exchange.get_status({exchange})"),
                format!("(vm.http.exchange.get_status {exchange})"),
            )))
        }
        "http_exchange_get_header" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_header");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let name = block_value(block, "name", "x-upstream");
            Some(Ok(assignment_action(
                &var,
                format!(
                    "vm::http::exchange::get_header({exchange}, {})",
                    rust_string(name)
                ),
                format!(
                    "vm.http.exchange.get_header({exchange}, {})",
                    js_string(name)
                ),
                format!(
                    "vm.http.exchange.get_header({exchange}, {})",
                    lua_string(name)
                ),
                format!(
                    "(vm.http.exchange.get_header {exchange} {})",
                    scheme_string(name)
                ),
            )))
        }
        "http_exchange_get_headers" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_headers");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::http::exchange::get_headers({exchange})"),
                format!("vm.http.exchange.get_headers({exchange})"),
                format!("vm.http.exchange.get_headers({exchange})"),
                format!("(vm.http.exchange.get_headers {exchange})"),
            )))
        }
        "http_exchange_get_body" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_body");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::http::exchange::get_body({exchange})"),
                format!("vm.http.exchange.get_body({exchange})"),
                format!("vm.http.exchange.get_body({exchange})"),
                format!("(vm.http.exchange.get_body {exchange})"),
            )))
        }
        "http_exchange_get_http_version" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_http_version");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::http::exchange::get_http_version({exchange})"),
                format!("vm.http.exchange.get_http_version({exchange})"),
                format!("vm.http.exchange.get_http_version({exchange})"),
                format!("(vm.http.exchange.get_http_version {exchange})"),
            )))
        }
        "http_exchange_body_next_chunk" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_chunk");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "1024"), "1024");
            Some(Ok(assignment_action(
                &var,
                format!("vm::http::exchange::body::next_chunk({exchange}, {max_bytes})"),
                format!("vm.http.exchange.body.next_chunk({exchange}, {max_bytes})"),
                format!("vm.http.exchange.body.next_chunk({exchange}, {max_bytes})"),
                format!("(vm.http.exchange.body.next_chunk {exchange} {max_bytes})"),
            )))
        }
        "http_exchange_body_eof" => {
            let var = sanitize_identifier(block.values.get("var"), "exchange_body_eof");
            let exchange = render_number_expr(block_value(block, "exchange", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::http::exchange::body::eof({exchange})"),
                format!("vm.http.exchange.body.eof({exchange})"),
                format!("vm.http.exchange.body.eof({exchange})"),
                format!("(vm.http.exchange.body.eof {exchange})"),
            )))
        }
        _ => None,
    }
}

fn additional_transport_flow_action(
    block: &UiBlockInstance,
) -> Option<Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)>> {
    match block.block_id.as_str() {
        "tcp_stream_is_present" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_present");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::is_present({stream})"),
                format!("vm.tcp.stream.is_present({stream})"),
                format!("vm.tcp.stream.is_present({stream})"),
                format!("(vm.tcp.stream.is_present {stream})"),
            )))
        }
        "tcp_stream_bind" => {
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            let local_addr = block_value(block, "local_addr", "127.0.0.1:0");
            Some(Ok(statement_action(
                format!(
                    "vm::tcp::stream::bind({stream}, {});",
                    render_expr_rss(local_addr)
                ),
                format!(
                    "vm.tcp.stream.bind({stream}, {});",
                    render_expr_js(local_addr)
                ),
                format!(
                    "vm.tcp.stream.bind({stream}, {})",
                    render_expr_lua(local_addr)
                ),
                format!(
                    "(vm.tcp.stream.bind {stream} {})",
                    render_expr_scheme(local_addr)
                ),
            )))
        }
        "tcp_stream_set_target" => {
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            let target = block_value(block, "target", "127.0.0.1:8080");
            Some(Ok(statement_action(
                format!(
                    "vm::tcp::stream::set_target({stream}, {});",
                    render_expr_rss(target)
                ),
                format!(
                    "vm.tcp.stream.set_target({stream}, {});",
                    render_expr_js(target)
                ),
                format!(
                    "vm.tcp.stream.set_target({stream}, {})",
                    render_expr_lua(target)
                ),
                format!(
                    "(vm.tcp.stream.set_target {stream} {})",
                    render_expr_scheme(target)
                ),
            )))
        }
        "tcp_stream_connect" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_connected");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::connect({stream})"),
                format!("vm.tcp.stream.connect({stream})"),
                format!("vm.tcp.stream.connect({stream})"),
                format!("(vm.tcp.stream.connect {stream})"),
            )))
        }
        "tcp_stream_get_phase" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_phase");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::get_phase({stream})"),
                format!("vm.tcp.stream.get_phase({stream})"),
                format!("vm.tcp.stream.get_phase({stream})"),
                format!("(vm.tcp.stream.get_phase {stream})"),
            )))
        }
        "tcp_stream_get_local_addr" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_local_addr");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::get_local_addr({stream})"),
                format!("vm.tcp.stream.get_local_addr({stream})"),
                format!("vm.tcp.stream.get_local_addr({stream})"),
                format!("(vm.tcp.stream.get_local_addr {stream})"),
            )))
        }
        "tcp_stream_get_peer_addr" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_peer_addr");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::get_peer_addr({stream})"),
                format!("vm.tcp.stream.get_peer_addr({stream})"),
                format!("vm.tcp.stream.get_peer_addr({stream})"),
                format!("(vm.tcp.stream.get_peer_addr {stream})"),
            )))
        }
        "tcp_stream_read" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_text");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "1024"), "1024");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::read({stream}, {max_bytes})"),
                format!("vm.tcp.stream.read({stream}, {max_bytes})"),
                format!("vm.tcp.stream.read({stream}, {max_bytes})"),
                format!("(vm.tcp.stream.read {stream} {max_bytes})"),
            )))
        }
        "tcp_stream_peek" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_peek");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "1024"), "1024");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::peek({stream}, {max_bytes})"),
                format!("vm.tcp.stream.peek({stream}, {max_bytes})"),
                format!("vm.tcp.stream.peek({stream}, {max_bytes})"),
                format!("(vm.tcp.stream.peek {stream} {max_bytes})"),
            )))
        }
        "tcp_stream_write" => {
            let var = sanitize_identifier(block.values.get("var"), "bytes_written");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            let value = block_value(block, "value", "hello");
            Some(Ok(assignment_action(
                &var,
                format!(
                    "vm::tcp::stream::write({stream}, {})",
                    render_expr_rss(value)
                ),
                format!("vm.tcp.stream.write({stream}, {})", render_expr_js(value)),
                format!("vm.tcp.stream.write({stream}, {})", render_expr_lua(value)),
                format!(
                    "(vm.tcp.stream.write {stream} {})",
                    render_expr_scheme(value)
                ),
            )))
        }
        "tcp_stream_eof" => {
            let var = sanitize_identifier(block.values.get("var"), "stream_eof");
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tcp::stream::eof({stream})"),
                format!("vm.tcp.stream.eof({stream})"),
                format!("vm.tcp.stream.eof({stream})"),
                format!("(vm.tcp.stream.eof {stream})"),
            )))
        }
        "tcp_stream_close" => {
            let stream = render_number_expr(block_value(block, "stream", "1"), "1");
            Some(Ok(statement_action(
                format!("vm::tcp::stream::close({stream});"),
                format!("vm.tcp.stream.close({stream});"),
                format!("vm.tcp.stream.close({stream})"),
                format!("(vm.tcp.stream.close {stream})"),
            )))
        }
        "tls_session_is_present" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_present");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::is_present({session})"),
                format!("vm.tls.session.is_present({session})"),
                format!("vm.tls.session.is_present({session})"),
                format!("(vm.tls.session.is_present {session})"),
            )))
        }
        "tls_session_handshake" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_handshake_ok");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::handshake({session})"),
                format!("vm.tls.session.handshake({session})"),
                format!("vm.tls.session.handshake({session})"),
                format!("(vm.tls.session.handshake {session})"),
            )))
        }
        "tls_session_set_alpn" => {
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            let protocols = block_value(block, "protocols", "h2,http/1.1");
            Some(Ok(statement_action(
                format!(
                    "vm::tls::session::set_alpn({session}, {});",
                    render_expr_rss(protocols)
                ),
                format!(
                    "vm.tls.session.set_alpn({session}, {});",
                    render_expr_js(protocols)
                ),
                format!(
                    "vm.tls.session.set_alpn({session}, {})",
                    render_expr_lua(protocols)
                ),
                format!(
                    "(vm.tls.session.set_alpn {session} {})",
                    render_expr_scheme(protocols)
                ),
            )))
        }
        "tls_session_set_verify" => {
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            let verify = block_value(block, "verify", "true");
            Some(Ok(statement_action(
                format!(
                    "vm::tls::session::set_verify({session}, {});",
                    bool_expr_rss(verify, "true")
                ),
                format!(
                    "vm.tls.session.set_verify({session}, {});",
                    bool_expr_js(verify, "true")
                ),
                format!(
                    "vm.tls.session.set_verify({session}, {})",
                    bool_expr_lua(verify, "true")
                ),
                format!(
                    "(vm.tls.session.set_verify {session} {})",
                    bool_expr_scheme(verify, "true")
                ),
            )))
        }
        "tls_session_set_verify_hostname" => {
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            let verify = block_value(block, "verify", "true");
            Some(Ok(statement_action(
                format!(
                    "vm::tls::session::set_verify_hostname({session}, {});",
                    bool_expr_rss(verify, "true")
                ),
                format!(
                    "vm.tls.session.set_verify_hostname({session}, {});",
                    bool_expr_js(verify, "true")
                ),
                format!(
                    "vm.tls.session.set_verify_hostname({session}, {})",
                    bool_expr_lua(verify, "true")
                ),
                format!(
                    "(vm.tls.session.set_verify_hostname {session} {})",
                    bool_expr_scheme(verify, "true")
                ),
            )))
        }
        "tls_session_set_trusted_certificate"
        | "tls_session_set_client_certificate"
        | "tls_session_set_client_private_key"
        | "tls_session_set_server_certificate"
        | "tls_session_set_server_private_key" => {
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            let value_key = if block.block_id.ends_with("private_key") {
                "private_key_pem"
            } else {
                "certificate_pem"
            };
            let value = block_value(block, value_key, "-----BEGIN CERTIFICATE-----");
            let fn_name = block.block_id.trim_start_matches("tls_session_");
            Some(Ok(statement_action(
                format!(
                    "vm::tls::session::{}({session}, {});",
                    fn_name,
                    render_expr_rss(value)
                ),
                format!(
                    "vm.tls.session.{}({session}, {});",
                    fn_name,
                    render_expr_js(value)
                ),
                format!(
                    "vm.tls.session.{}({session}, {})",
                    fn_name,
                    render_expr_lua(value)
                ),
                format!(
                    "(vm.tls.session.{} {session} {})",
                    fn_name,
                    render_expr_scheme(value)
                ),
            )))
        }
        "tls_session_set_sni" => {
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            let enabled = block_value(block, "enabled", "true");
            Some(Ok(statement_action(
                format!(
                    "vm::tls::session::set_sni({session}, {});",
                    bool_expr_rss(enabled, "true")
                ),
                format!(
                    "vm.tls.session.set_sni({session}, {});",
                    bool_expr_js(enabled, "true")
                ),
                format!(
                    "vm.tls.session.set_sni({session}, {})",
                    bool_expr_lua(enabled, "true")
                ),
                format!(
                    "(vm.tls.session.set_sni {session} {})",
                    bool_expr_scheme(enabled, "true")
                ),
            )))
        }
        "tls_session_set_min_version" | "tls_session_set_max_version" => {
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            let version = block_value(block, "version", "1.2");
            let fn_name = block.block_id.trim_start_matches("tls_session_");
            Some(Ok(statement_action(
                format!(
                    "vm::tls::session::{}({session}, {});",
                    fn_name,
                    render_expr_rss(version)
                ),
                format!(
                    "vm.tls.session.{}({session}, {});",
                    fn_name,
                    render_expr_js(version)
                ),
                format!(
                    "vm.tls.session.{}({session}, {})",
                    fn_name,
                    render_expr_lua(version)
                ),
                format!(
                    "(vm.tls.session.{} {session} {})",
                    fn_name,
                    render_expr_scheme(version)
                ),
            )))
        }
        "tls_session_get_peer_name" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_peer_name");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::get_peer_name({session})"),
                format!("vm.tls.session.get_peer_name({session})"),
                format!("vm.tls.session.get_peer_name({session})"),
                format!("(vm.tls.session.get_peer_name {session})"),
            )))
        }
        "tls_session_get_alpn" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_alpn");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::get_alpn({session})"),
                format!("vm.tls.session.get_alpn({session})"),
                format!("vm.tls.session.get_alpn({session})"),
                format!("(vm.tls.session.get_alpn {session})"),
            )))
        }
        "tls_session_get_phase" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_phase");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::get_phase({session})"),
                format!("vm.tls.session.get_phase({session})"),
                format!("vm.tls.session.get_phase({session})"),
                format!("(vm.tls.session.get_phase {session})"),
            )))
        }
        "tls_session_get_peer_certificate" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_peer_certificate");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::get_peer_certificate({session})"),
                format!("vm.tls.session.get_peer_certificate({session})"),
                format!("vm.tls.session.get_peer_certificate({session})"),
                format!("(vm.tls.session.get_peer_certificate {session})"),
            )))
        }
        "tls_session_is_session_reused" => {
            let var = sanitize_identifier(block.values.get("var"), "tls_reused");
            let session = render_number_expr(block_value(block, "session", "1"), "1");
            Some(Ok(assignment_action(
                &var,
                format!("vm::tls::session::is_session_reused({session})"),
                format!("vm.tls.session.is_session_reused({session})"),
                format!("vm.tls.session.is_session_reused({session})"),
                format!("(vm.tls.session.is_session_reused {session})"),
            )))
        }
        _ => None,
    }
}

fn additional_realtime_flow_action(
    block: &UiBlockInstance,
) -> Option<Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)>> {
    match block.block_id.as_str() {
        "websocket_connection_is_present"
        | "websocket_connection_connect"
        | "websocket_connection_get_phase"
        | "websocket_connection_get_subprotocol"
        | "websocket_connection_send_text"
        | "websocket_connection_read_text"
        | "websocket_connection_send_binary_base64"
        | "websocket_connection_read_binary_base64"
        | "websocket_connection_eof"
        | "webrtc_connection_is_present"
        | "webrtc_connection_create_offer"
        | "webrtc_connection_create_answer"
        | "webrtc_connection_connect"
        | "webrtc_connection_get_phase"
        | "webrtc_connection_send_text"
        | "webrtc_connection_read_text"
        | "webrtc_connection_send_binary_base64"
        | "webrtc_connection_read_binary_base64"
        | "webrtc_connection_eof"
        | "udp_socket_is_present"
        | "udp_socket_connect"
        | "udp_socket_get_phase"
        | "udp_socket_get_local_addr"
        | "udp_socket_get_peer_addr"
        | "udp_socket_send_text"
        | "udp_socket_recv_text"
        | "udp_socket_send_binary_base64"
        | "udp_socket_recv_binary_base64" => Some(render_generic_assignment_action(block)),
        "websocket_connection_set_target"
        | "websocket_connection_set_header"
        | "websocket_connection_set_subprotocols"
        | "websocket_connection_close"
        | "webrtc_connection_set_ice_servers"
        | "webrtc_connection_set_data_channel_label"
        | "webrtc_connection_set_remote_description"
        | "webrtc_connection_close"
        | "udp_socket_bind"
        | "udp_socket_set_target"
        | "udp_socket_close" => Some(render_generic_statement_action(block)),
        _ => None,
    }
}

fn additional_proxy_flow_action(
    block: &UiBlockInstance,
) -> Option<Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)>> {
    match block.block_id.as_str() {
        "proxy_pipe" | "proxy_bridge" => Some(render_generic_assignment_action(block)),
        _ => None,
    }
}

fn push_value_assignment(
    rss: &mut Vec<String>,
    js: &mut Vec<String>,
    lua: &mut Vec<String>,
    scm: &mut Vec<String>,
    var: &str,
    rss_expr: String,
    js_expr: String,
    lua_expr: String,
    scm_expr: String,
) {
    rss.push(format!("let {var} = {rss_expr};"));
    js.push(format!("let {var} = {js_expr};"));
    lua.push(format!("local {var} = {lua_expr}"));
    scm.push(format!("(define {var} {scm_expr})"));
}

fn assignment_action(
    var: &str,
    rss_expr: String,
    js_expr: String,
    lua_expr: String,
    scm_expr: String,
) -> FlowActionStatement {
    FlowActionStatement {
        rustscript: format!("let {var} = {rss_expr};"),
        javascript: format!("let {var} = {js_expr};"),
        lua: format!("local {var} = {lua_expr}"),
        scheme: format!("(define {var} {scm_expr})"),
    }
}

fn statement_action(
    rustscript: String,
    javascript: String,
    lua: String,
    scheme: String,
) -> FlowActionStatement {
    FlowActionStatement {
        rustscript,
        javascript,
        lua,
        scheme,
    }
}

fn bool_expr_rss(raw: &str, fallback: &str) -> String {
    render_bool_expr(raw, fallback, "true", "false")
}

fn bool_expr_js(raw: &str, fallback: &str) -> String {
    render_bool_expr(raw, fallback, "true", "false")
}

fn bool_expr_lua(raw: &str, fallback: &str) -> String {
    render_bool_expr(raw, fallback, "true", "false")
}

fn bool_expr_scheme(raw: &str, fallback: &str) -> String {
    render_bool_expr(raw, fallback, "#t", "#f")
}

fn render_generic_assignment_action(
    block: &UiBlockInstance,
) -> Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)> {
    let var = sanitize_identifier(block.values.get("var"), "value");
    let (rss_expr, js_expr, lua_expr, scm_expr) = generic_vm_call(block)?;
    Ok(assignment_action(
        &var, rss_expr, js_expr, lua_expr, scm_expr,
    ))
}

fn render_generic_statement_action(
    block: &UiBlockInstance,
) -> Result<FlowActionStatement, (StatusCode, Json<ErrorResponse>)> {
    let (rss_expr, js_expr, lua_expr, scm_expr) = generic_vm_call(block)?;
    Ok(statement_action(
        format!("{rss_expr};"),
        format!("{js_expr};"),
        lua_expr,
        scm_expr,
    ))
}

fn generic_vm_call(
    block: &UiBlockInstance,
) -> Result<(String, String, String, String), (StatusCode, Json<ErrorResponse>)> {
    let rss_path =
        vm_rss_path(&block.block_id).ok_or_else(|| bad_request("unsupported UI ABI block"))?;
    let js_path =
        vm_js_path(&block.block_id).ok_or_else(|| bad_request("unsupported UI ABI block"))?;
    let lua_path = js_path.clone();
    let scm_path =
        vm_scheme_path(&block.block_id).ok_or_else(|| bad_request("unsupported UI ABI block"))?;

    let (rss_args, js_args, lua_args, scm_args) = generic_vm_args(block);
    Ok((
        format!("{rss_path}({})", rss_args.join(", ")),
        format!("{js_path}({})", js_args.join(", ")),
        format!("{lua_path}({})", lua_args.join(", ")),
        format!("({scm_path}{})", scheme_call_suffix(&scm_args)),
    ))
}

fn generic_vm_args(
    block: &UiBlockInstance,
) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    match block.block_id.as_str() {
        "websocket_connection_is_present"
        | "websocket_connection_connect"
        | "websocket_connection_get_phase"
        | "websocket_connection_get_subprotocol"
        | "websocket_connection_read_text"
        | "websocket_connection_read_binary_base64"
        | "websocket_connection_eof"
        | "webrtc_connection_is_present"
        | "webrtc_connection_create_offer"
        | "webrtc_connection_create_answer"
        | "webrtc_connection_connect"
        | "webrtc_connection_get_phase"
        | "webrtc_connection_read_text"
        | "webrtc_connection_read_binary_base64"
        | "webrtc_connection_eof" => {
            let connection = render_number_expr(block_value(block, "connection", "1"), "1");
            (
                vec![connection.clone()],
                vec![connection.clone()],
                vec![connection.clone()],
                vec![connection],
            )
        }
        "udp_socket_is_present"
        | "udp_socket_connect"
        | "udp_socket_get_phase"
        | "udp_socket_get_local_addr"
        | "udp_socket_get_peer_addr"
        | "udp_socket_close" => {
            let socket = render_number_expr(block_value(block, "socket", "1"), "1");
            (
                vec![socket.clone()],
                vec![socket.clone()],
                vec![socket.clone()],
                vec![socket],
            )
        }
        "websocket_connection_set_target" => {
            args_num_expr(block, "connection", "1", "target", "ws://127.0.0.1:8080")
        }
        "websocket_connection_set_header" => {
            args_num_name_expr(block, "connection", "1", "name", "x-ws", "value", "1")
        }
        "websocket_connection_set_subprotocols" => {
            args_num_expr(block, "connection", "1", "protocols", "chat,json")
        }
        "websocket_connection_send_text" => {
            args_num_expr(block, "connection", "1", "text", "hello")
        }
        "websocket_connection_send_binary_base64" => {
            args_num_expr(block, "connection", "1", "payload", "aGVsbG8=")
        }
        "websocket_connection_close" => {
            let connection = render_number_expr(block_value(block, "connection", "1"), "1");
            let code = render_number_expr(block_value(block, "code", "1000"), "1000");
            let reason = block_value(block, "reason", "done");
            (
                vec![connection.clone(), code.clone(), render_expr_rss(reason)],
                vec![connection.clone(), code.clone(), render_expr_js(reason)],
                vec![connection.clone(), code.clone(), render_expr_lua(reason)],
                vec![connection, code, render_expr_scheme(reason)],
            )
        }
        "webrtc_connection_set_ice_servers" => args_num_expr(
            block,
            "connection",
            "1",
            "urls",
            "stun:stun.l.google.com:19302",
        ),
        "webrtc_connection_set_data_channel_label" => {
            args_num_expr(block, "connection", "1", "label", "data")
        }
        "webrtc_connection_set_remote_description" => args_num_expr(
            block,
            "connection",
            "1",
            "description_json",
            "{\"type\":\"offer\",\"sdp\":\"...\"}",
        ),
        "webrtc_connection_send_text" => args_num_expr(block, "connection", "1", "text", "hello"),
        "webrtc_connection_send_binary_base64" => {
            args_num_expr(block, "connection", "1", "payload", "aGVsbG8=")
        }
        "webrtc_connection_close" => {
            let connection = render_number_expr(block_value(block, "connection", "1"), "1");
            (
                vec![connection.clone()],
                vec![connection.clone()],
                vec![connection.clone()],
                vec![connection],
            )
        }
        "udp_socket_bind" => args_num_expr(block, "socket", "1", "local_addr", "127.0.0.1:0"),
        "udp_socket_set_target" => args_num_expr(block, "socket", "1", "target", "127.0.0.1:8080"),
        "udp_socket_send_text" => args_num_expr(block, "socket", "1", "text", "hello"),
        "udp_socket_recv_text" | "udp_socket_recv_binary_base64" => {
            let socket = render_number_expr(block_value(block, "socket", "1"), "1");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "1024"), "1024");
            (
                vec![socket.clone(), max_bytes.clone()],
                vec![socket.clone(), max_bytes.clone()],
                vec![socket.clone(), max_bytes.clone()],
                vec![socket, max_bytes],
            )
        }
        "udp_socket_send_binary_base64" => {
            args_num_expr(block, "socket", "1", "payload", "aGVsbG8=")
        }
        "proxy_pipe" | "proxy_bridge" => {
            let left_key = if block.block_id == "proxy_pipe" {
                "source"
            } else {
                "left"
            };
            let right_key = if block.block_id == "proxy_pipe" {
                "destination"
            } else {
                "right"
            };
            let left = render_number_expr(block_value(block, left_key, "1"), "1");
            let right = render_number_expr(block_value(block, right_key, "2"), "2");
            let max_bytes = render_number_expr(block_value(block, "max_bytes", "65536"), "65536");
            (
                vec![left.clone(), right.clone(), max_bytes.clone()],
                vec![left.clone(), right.clone(), max_bytes.clone()],
                vec![left.clone(), right.clone(), max_bytes.clone()],
                vec![left, right, max_bytes],
            )
        }
        _ => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
    }
}

fn args_num_expr(
    block: &UiBlockInstance,
    handle_key: &str,
    handle_fallback: &str,
    value_key: &str,
    value_fallback: &str,
) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let handle = render_number_expr(
        block_value(block, handle_key, handle_fallback),
        handle_fallback,
    );
    let value = block_value(block, value_key, value_fallback);
    (
        vec![handle.clone(), render_expr_rss(value)],
        vec![handle.clone(), render_expr_js(value)],
        vec![handle.clone(), render_expr_lua(value)],
        vec![handle, render_expr_scheme(value)],
    )
}

fn args_num_name_expr(
    block: &UiBlockInstance,
    handle_key: &str,
    handle_fallback: &str,
    name_key: &str,
    name_fallback: &str,
    value_key: &str,
    value_fallback: &str,
) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let handle = render_number_expr(
        block_value(block, handle_key, handle_fallback),
        handle_fallback,
    );
    let name = block_value(block, name_key, name_fallback);
    let value = block_value(block, value_key, value_fallback);
    (
        vec![handle.clone(), rust_string(name), render_expr_rss(value)],
        vec![handle.clone(), js_string(name), render_expr_js(value)],
        vec![handle.clone(), lua_string(name), render_expr_lua(value)],
        vec![handle, scheme_string(name), render_expr_scheme(value)],
    )
}

fn vm_rss_path(block_id: &str) -> Option<String> {
    match_namespace_path(block_id, "::")
}

fn vm_js_path(block_id: &str) -> Option<String> {
    match_namespace_path(block_id, ".")
}

fn vm_scheme_path(block_id: &str) -> Option<String> {
    match_namespace_path(block_id, ".")
}

fn match_namespace_path(block_id: &str, sep: &str) -> Option<String> {
    let prefix = if sep == "::" { "vm::" } else { "vm." };
    let path = match block_id.split('_').collect::<Vec<_>>().as_slice() {
        ["websocket", "connection", rest @ ..] => Some(format!(
            "{}websocket{sep}connection{sep}{}",
            prefix,
            rest.join("_")
        )),
        ["webrtc", "connection", rest @ ..] => Some(format!(
            "{}webrtc{sep}connection{sep}{}",
            prefix,
            rest.join("_")
        )),
        ["udp", "socket", rest @ ..] => {
            Some(format!("{}udp{sep}socket{sep}{}", prefix, rest.join("_")))
        }
        ["proxy", "stream", rest @ ..] => {
            Some(format!("{}proxy{sep}stream{sep}{}", prefix, rest.join("_")))
        }
        ["proxy", rest @ ..] => Some(format!("{}proxy{sep}{}", prefix, rest.join("_"))),
        _ => None,
    }?;
    Some(path)
}

fn scheme_call_suffix(args: &[String]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!(" {}", args.join(" "))
    }
}
