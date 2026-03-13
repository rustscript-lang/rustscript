use super::*;

pub(super) fn additional_abi_blocks() -> Vec<UiBlockDefinition> {
    let mut blocks = Vec::new();
    blocks.extend(http_exchange_blocks());
    blocks.extend(http_upstream_helper_blocks());
    blocks.extend(tcp_stream_blocks());
    blocks.extend(tls_session_blocks());
    blocks.extend(websocket_connection_blocks());
    blocks.extend(webrtc_connection_blocks());
    blocks.extend(udp_socket_blocks());
    blocks.extend(proxy_stream_blocks());
    blocks
}

fn http_exchange_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "http_exchange_new",
            "HTTP Exchange New",
            "http_exchange",
            "Create a new HTTP exchange handle.",
            vec![var_input("var", "Variable", "exchange", "exchange")],
        ),
        value_block(
            "http_exchange_default_upstream",
            "HTTP Exchange Default Upstream",
            "http_exchange",
            "Get the default upstream HTTP exchange handle.",
            vec![var_input(
                "var",
                "Variable",
                "default_exchange",
                "default_exchange",
            )],
        ),
        flow_block(
            "http_downstream_attach_transport",
            "HTTP Downstream Attach Transport",
            "http_downstream",
            "Attach downstream transport to HTTP processing.",
            vec![],
        ),
        flow_block(
            "http_exchange_send",
            "HTTP Exchange Send",
            "http_exchange",
            "Send an HTTP exchange.",
            vec![handle_input("exchange", "Exchange", "1")],
        ),
        flow_block(
            "http_exchange_set_header",
            "HTTP Exchange Set Header",
            "http_exchange",
            "Set an HTTP exchange request header.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("name", "Header Name", "x-added", "x-added", false),
                text_input("value", "Value", "1", "1 or $var", true),
            ],
        ),
        flow_block(
            "http_exchange_add_header",
            "HTTP Exchange Add Header",
            "http_exchange",
            "Append an HTTP exchange request header.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("name", "Header Name", "set-cookie", "set-cookie", false),
                text_input("value", "Value", "a=1", "a=1 or $var", true),
            ],
        ),
        flow_block(
            "http_exchange_clear_header",
            "HTTP Exchange Clear Header",
            "http_exchange",
            "Clear an HTTP exchange request header.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("name", "Header Name", "x-remove", "x-remove", false),
            ],
        ),
        flow_block(
            "http_exchange_set_method",
            "HTTP Exchange Set Method",
            "http_exchange",
            "Set an HTTP exchange request method.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("method", "Method", "GET", "GET or $var", true),
            ],
        ),
        flow_block(
            "http_exchange_set_path",
            "HTTP Exchange Set Path",
            "http_exchange",
            "Set an HTTP exchange request path.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("path", "Path", "/", "/ or $var", true),
            ],
        ),
        flow_block(
            "http_exchange_set_query",
            "HTTP Exchange Set Query",
            "http_exchange",
            "Set an HTTP exchange request query string.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("query", "Query", "x=1", "x=1 or $var", true),
            ],
        ),
        flow_block(
            "http_exchange_set_target",
            "HTTP Exchange Set Target",
            "http_exchange",
            "Set an HTTP exchange upstream target.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input(
                    "target",
                    "Target",
                    "127.0.0.1:8080",
                    "127.0.0.1:8080 or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "http_exchange_attach_tcp",
            "HTTP Exchange Attach TCP",
            "http_exchange",
            "Attach an HTTP exchange to a TCP stream.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_block(
            "http_exchange_attach_tls_plaintext",
            "HTTP Exchange Attach TLS Plaintext",
            "http_exchange",
            "Attach an HTTP exchange to a plaintext TLS session.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_block(
            "http_exchange_set_body",
            "HTTP Exchange Set Body",
            "http_exchange",
            "Set an HTTP exchange request body.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("value", "Body", "payload", "payload or $var", true),
            ],
        ),
        flow_block(
            "http_exchange_set_query_arg",
            "HTTP Exchange Set Query Arg",
            "http_exchange",
            "Set one HTTP exchange request query parameter.",
            vec![
                handle_input("exchange", "Exchange", "1"),
                text_input("name", "Query Name", "id", "id", false),
                text_input("value", "Value", "1", "1 or $var", true),
            ],
        ),
        flow_value_block(
            "http_exchange_get_status",
            "HTTP Exchange Get Status",
            "http_exchange",
            "Read an HTTP exchange response status.",
            vec![
                var_input("var", "Variable", "exchange_status", "exchange_status"),
                handle_input("exchange", "Exchange", "1"),
            ],
        ),
        flow_value_block(
            "http_exchange_get_header",
            "HTTP Exchange Get Header",
            "http_exchange",
            "Read one HTTP exchange response header.",
            vec![
                var_input("var", "Variable", "exchange_header", "exchange_header"),
                handle_input("exchange", "Exchange", "1"),
                text_input("name", "Header Name", "x-upstream", "x-upstream", false),
            ],
        ),
        flow_value_block(
            "http_exchange_get_headers",
            "HTTP Exchange Get Headers",
            "http_exchange",
            "Read all HTTP exchange response headers.",
            vec![
                var_input("var", "Variable", "exchange_headers", "exchange_headers"),
                handle_input("exchange", "Exchange", "1"),
            ],
        ),
        flow_value_block(
            "http_exchange_get_body",
            "HTTP Exchange Get Body",
            "http_exchange",
            "Read an HTTP exchange response body.",
            vec![
                var_input("var", "Variable", "exchange_body", "exchange_body"),
                handle_input("exchange", "Exchange", "1"),
            ],
        ),
        flow_value_block(
            "http_exchange_get_http_version",
            "HTTP Exchange Get HTTP Version",
            "http_exchange",
            "Read an HTTP exchange response HTTP version.",
            vec![
                var_input(
                    "var",
                    "Variable",
                    "exchange_http_version",
                    "exchange_http_version",
                ),
                handle_input("exchange", "Exchange", "1"),
            ],
        ),
        flow_value_block(
            "http_exchange_body_next_chunk",
            "HTTP Exchange Body Next Chunk",
            "http_exchange",
            "Read the next HTTP exchange response body chunk.",
            vec![
                var_input("var", "Variable", "exchange_chunk", "exchange_chunk"),
                handle_input("exchange", "Exchange", "1"),
                number_input("max_bytes", "Max Bytes", "1024", "1024 or $var", true),
            ],
        ),
        flow_value_block(
            "http_exchange_body_eof",
            "HTTP Exchange Body EOF",
            "http_exchange",
            "Read whether the HTTP exchange response body reached EOF.",
            vec![
                var_input("var", "Variable", "exchange_body_eof", "exchange_body_eof"),
                handle_input("exchange", "Exchange", "1"),
            ],
        ),
    ]
}

fn http_upstream_helper_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "http_upstream_as_stream",
            "HTTP Upstream As Stream",
            "http_upstream",
            "Project the default upstream HTTP exchange as a proxy stream via the stdlib wrapper.",
            vec![var_input(
                "var",
                "Variable",
                "upstream_stream",
                "upstream_stream",
            )],
        ),
        value_block(
            "get_upstream_response_http_version",
            "Get Upstream Response HTTP Version",
            "http_upstream_response",
            "Read upstream response HTTP version via the stdlib wrapper.",
            vec![var_input(
                "var",
                "Variable",
                "upstream_http_version",
                "upstream_http_version",
            )],
        ),
        value_block(
            "get_upstream_response_next_chunk",
            "Get Upstream Response Chunk",
            "http_upstream_response",
            "Read the next upstream response body chunk via the stdlib wrapper.",
            vec![
                var_input("var", "Variable", "upstream_chunk", "upstream_chunk"),
                number_input("max_bytes", "Max Bytes", "1024", "1024 or $var", true),
            ],
        ),
        value_block(
            "get_upstream_response_eof",
            "Get Upstream Response EOF",
            "http_upstream_response",
            "Read whether the upstream response body reached EOF via the stdlib wrapper.",
            vec![var_input(
                "var",
                "Variable",
                "upstream_body_eof",
                "upstream_body_eof",
            )],
        ),
        value_block(
            "read_upstream_response_line",
            "Read Upstream Response Line",
            "http_upstream_response",
            "Read one upstream response line via the stdlib wrapper.",
            vec![var_input(
                "var",
                "Variable",
                "upstream_line",
                "upstream_line",
            )],
        ),
        value_block(
            "read_upstream_response_all",
            "Read Upstream Response All",
            "http_upstream_response",
            "Read the full upstream response stream via the stdlib wrapper.",
            vec![var_input("var", "Variable", "upstream_all", "upstream_all")],
        ),
    ]
}

fn tcp_stream_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "tcp_stream_downstream",
            "TCP Stream Downstream",
            "tcp_stream",
            "Get the downstream TCP stream handle.",
            vec![var_input(
                "var",
                "Variable",
                "downstream_stream",
                "downstream_stream",
            )],
        ),
        value_block(
            "tcp_stream_default_upstream",
            "TCP Stream Default Upstream",
            "tcp_stream",
            "Get the default upstream TCP stream handle.",
            vec![var_input(
                "var",
                "Variable",
                "default_tcp_upstream",
                "default_tcp_upstream",
            )],
        ),
        value_block(
            "tcp_stream_new",
            "TCP Stream New",
            "tcp_stream",
            "Create a new TCP stream handle.",
            vec![var_input("var", "Variable", "tcp_stream", "tcp_stream")],
        ),
        flow_value_block(
            "tcp_stream_is_present",
            "TCP Stream Is Present",
            "tcp_stream",
            "Check whether a TCP stream handle is present.",
            vec![
                var_input("var", "Variable", "stream_present", "stream_present"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_block(
            "tcp_stream_bind",
            "TCP Stream Bind",
            "tcp_stream",
            "Bind a TCP stream to a local address.",
            vec![
                handle_input("stream", "Stream", "1"),
                text_input(
                    "local_addr",
                    "Local Addr",
                    "127.0.0.1:0",
                    "127.0.0.1:0 or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tcp_stream_set_target",
            "TCP Stream Set Target",
            "tcp_stream",
            "Set a TCP stream target address.",
            vec![
                handle_input("stream", "Stream", "1"),
                text_input(
                    "target",
                    "Target",
                    "127.0.0.1:8080",
                    "127.0.0.1:8080 or $var",
                    true,
                ),
            ],
        ),
        flow_value_block(
            "tcp_stream_connect",
            "TCP Stream Connect",
            "tcp_stream",
            "Connect a TCP stream and store the boolean result.",
            vec![
                var_input("var", "Variable", "stream_connected", "stream_connected"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_value_block(
            "tcp_stream_get_phase",
            "TCP Stream Get Phase",
            "tcp_stream",
            "Read a TCP stream phase.",
            vec![
                var_input("var", "Variable", "stream_phase", "stream_phase"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_value_block(
            "tcp_stream_get_local_addr",
            "TCP Stream Get Local Addr",
            "tcp_stream",
            "Read a TCP stream local address.",
            vec![
                var_input("var", "Variable", "stream_local_addr", "stream_local_addr"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_value_block(
            "tcp_stream_get_peer_addr",
            "TCP Stream Get Peer Addr",
            "tcp_stream",
            "Read a TCP stream peer address.",
            vec![
                var_input("var", "Variable", "stream_peer_addr", "stream_peer_addr"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_value_block(
            "tcp_stream_read",
            "TCP Stream Read",
            "tcp_stream",
            "Read text from a TCP stream.",
            vec![
                var_input("var", "Variable", "stream_text", "stream_text"),
                handle_input("stream", "Stream", "1"),
                number_input("max_bytes", "Max Bytes", "1024", "1024 or $var", true),
            ],
        ),
        flow_value_block(
            "tcp_stream_peek",
            "TCP Stream Peek",
            "tcp_stream",
            "Peek text from a TCP stream.",
            vec![
                var_input("var", "Variable", "stream_peek", "stream_peek"),
                handle_input("stream", "Stream", "1"),
                number_input("max_bytes", "Max Bytes", "1024", "1024 or $var", true),
            ],
        ),
        flow_value_block(
            "tcp_stream_write",
            "TCP Stream Write",
            "tcp_stream",
            "Write text to a TCP stream and store the byte count.",
            vec![
                var_input("var", "Variable", "bytes_written", "bytes_written"),
                handle_input("stream", "Stream", "1"),
                text_input("value", "Value", "hello", "hello or $var", true),
            ],
        ),
        flow_value_block(
            "tcp_stream_eof",
            "TCP Stream EOF",
            "tcp_stream",
            "Read whether a TCP stream reached EOF.",
            vec![
                var_input("var", "Variable", "stream_eof", "stream_eof"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_block(
            "tcp_stream_close",
            "TCP Stream Close",
            "tcp_stream",
            "Close a TCP stream.",
            vec![handle_input("stream", "Stream", "1")],
        ),
    ]
}

fn tls_session_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "tls_session_from_socket",
            "TLS Session From Socket",
            "tls_session",
            "Create a TLS session from a TCP stream.",
            vec![
                var_input("var", "Variable", "tls_session", "tls_session"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        flow_value_block(
            "tls_session_is_present",
            "TLS Session Is Present",
            "tls_session",
            "Check whether a TLS session handle is present.",
            vec![
                var_input("var", "Variable", "tls_present", "tls_present"),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_value_block(
            "tls_session_handshake",
            "TLS Session Handshake",
            "tls_session",
            "Run a TLS handshake and store the boolean result.",
            vec![
                var_input("var", "Variable", "tls_handshake_ok", "tls_handshake_ok"),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_block(
            "tls_session_set_alpn",
            "TLS Session Set ALPN",
            "tls_session",
            "Set TLS ALPN protocols.",
            vec![
                handle_input("session", "Session", "1"),
                text_input(
                    "protocols",
                    "Protocols",
                    "h2,http/1.1",
                    "h2,http/1.1 or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tls_session_set_verify",
            "TLS Session Set Verify",
            "tls_session",
            "Enable or disable TLS certificate verification.",
            vec![
                handle_input("session", "Session", "1"),
                bool_input("verify", "Verify", "true", "true, false, or $var", true),
            ],
        ),
        flow_block(
            "tls_session_set_verify_hostname",
            "TLS Session Set Verify Hostname",
            "tls_session",
            "Enable or disable TLS hostname verification.",
            vec![
                handle_input("session", "Session", "1"),
                bool_input("verify", "Verify", "true", "true, false, or $var", true),
            ],
        ),
        flow_block(
            "tls_session_set_trusted_certificate",
            "TLS Session Set Trusted Certificate",
            "tls_session",
            "Set a trusted certificate PEM for TLS verification.",
            vec![
                handle_input("session", "Session", "1"),
                text_input(
                    "certificate_pem",
                    "Certificate PEM",
                    "-----BEGIN CERTIFICATE-----",
                    "-----BEGIN CERTIFICATE----- or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tls_session_set_client_certificate",
            "TLS Session Set Client Certificate",
            "tls_session",
            "Set a client certificate PEM for mTLS.",
            vec![
                handle_input("session", "Session", "1"),
                text_input(
                    "certificate_pem",
                    "Certificate PEM",
                    "-----BEGIN CERTIFICATE-----",
                    "-----BEGIN CERTIFICATE----- or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tls_session_set_client_private_key",
            "TLS Session Set Client Private Key",
            "tls_session",
            "Set a client private key PEM for mTLS.",
            vec![
                handle_input("session", "Session", "1"),
                text_input(
                    "private_key_pem",
                    "Private Key PEM",
                    "-----BEGIN PRIVATE KEY-----",
                    "-----BEGIN PRIVATE KEY----- or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tls_session_set_server_certificate",
            "TLS Session Set Server Certificate",
            "tls_session",
            "Set a server certificate PEM for downstream TLS.",
            vec![
                handle_input("session", "Session", "1"),
                text_input(
                    "certificate_pem",
                    "Certificate PEM",
                    "-----BEGIN CERTIFICATE-----",
                    "-----BEGIN CERTIFICATE----- or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tls_session_set_server_private_key",
            "TLS Session Set Server Private Key",
            "tls_session",
            "Set a server private key PEM for downstream TLS.",
            vec![
                handle_input("session", "Session", "1"),
                text_input(
                    "private_key_pem",
                    "Private Key PEM",
                    "-----BEGIN PRIVATE KEY-----",
                    "-----BEGIN PRIVATE KEY----- or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "tls_session_set_sni",
            "TLS Session Set SNI",
            "tls_session",
            "Enable or disable client-side SNI.",
            vec![
                handle_input("session", "Session", "1"),
                bool_input("enabled", "Enabled", "true", "true, false, or $var", true),
            ],
        ),
        flow_block(
            "tls_session_set_min_version",
            "TLS Session Set Min Version",
            "tls_session",
            "Set a minimum TLS version.",
            vec![
                handle_input("session", "Session", "1"),
                text_input("version", "Version", "1.2", "1.2 or $var", true),
            ],
        ),
        flow_block(
            "tls_session_set_max_version",
            "TLS Session Set Max Version",
            "tls_session",
            "Set a maximum TLS version.",
            vec![
                handle_input("session", "Session", "1"),
                text_input("version", "Version", "1.3", "1.3 or $var", true),
            ],
        ),
        flow_value_block(
            "tls_session_get_peer_name",
            "TLS Session Get Peer Name",
            "tls_session",
            "Read a TLS peer name.",
            vec![
                var_input("var", "Variable", "tls_peer_name", "tls_peer_name"),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_value_block(
            "tls_session_get_alpn",
            "TLS Session Get ALPN",
            "tls_session",
            "Read a negotiated TLS ALPN protocol.",
            vec![
                var_input("var", "Variable", "tls_alpn", "tls_alpn"),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_value_block(
            "tls_session_get_phase",
            "TLS Session Get Phase",
            "tls_session",
            "Read a TLS session phase.",
            vec![
                var_input("var", "Variable", "tls_phase", "tls_phase"),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_value_block(
            "tls_session_get_peer_certificate",
            "TLS Session Get Peer Certificate",
            "tls_session",
            "Read a peer certificate PEM from a TLS session.",
            vec![
                var_input(
                    "var",
                    "Variable",
                    "tls_peer_certificate",
                    "tls_peer_certificate",
                ),
                handle_input("session", "Session", "1"),
            ],
        ),
        flow_value_block(
            "tls_session_is_session_reused",
            "TLS Session Is Session Reused",
            "tls_session",
            "Read whether a TLS session was resumed.",
            vec![
                var_input("var", "Variable", "tls_reused", "tls_reused"),
                handle_input("session", "Session", "1"),
            ],
        ),
    ]
}

fn websocket_connection_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "websocket_connection_new",
            "WebSocket Connection New",
            "websocket_connection",
            "Create a new WebSocket connection handle.",
            vec![var_input("var", "Variable", "ws_conn", "ws_conn")],
        ),
        value_block(
            "websocket_connection_downstream",
            "WebSocket Connection Downstream",
            "websocket_connection",
            "Get the downstream WebSocket connection handle.",
            vec![var_input(
                "var",
                "Variable",
                "ws_downstream",
                "ws_downstream",
            )],
        ),
        value_block(
            "websocket_connection_default_upstream",
            "WebSocket Connection Default Upstream",
            "websocket_connection",
            "Get the default upstream WebSocket connection handle.",
            vec![var_input(
                "var",
                "Variable",
                "ws_default_upstream",
                "ws_default_upstream",
            )],
        ),
        flow_value_block(
            "websocket_connection_is_present",
            "WebSocket Connection Is Present",
            "websocket_connection",
            "Check whether a WebSocket connection handle is present.",
            vec![
                var_input("var", "Variable", "ws_present", "ws_present"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_block(
            "websocket_connection_set_target",
            "WebSocket Connection Set Target",
            "websocket_connection",
            "Set a WebSocket connection target.",
            vec![
                handle_input("connection", "Connection", "1"),
                text_input(
                    "target",
                    "Target",
                    "ws://127.0.0.1:8080",
                    "ws://127.0.0.1:8080 or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "websocket_connection_set_header",
            "WebSocket Connection Set Header",
            "websocket_connection",
            "Set a WebSocket handshake header.",
            vec![
                handle_input("connection", "Connection", "1"),
                text_input("name", "Header Name", "x-ws", "x-ws", false),
                text_input("value", "Value", "1", "1 or $var", true),
            ],
        ),
        flow_block(
            "websocket_connection_set_subprotocols",
            "WebSocket Connection Set Subprotocols",
            "websocket_connection",
            "Set desired WebSocket subprotocols.",
            vec![
                handle_input("connection", "Connection", "1"),
                text_input(
                    "protocols",
                    "Protocols",
                    "chat,json",
                    "chat,json or $var",
                    true,
                ),
            ],
        ),
        flow_value_block(
            "websocket_connection_connect",
            "WebSocket Connection Connect",
            "websocket_connection",
            "Connect a WebSocket connection and store the boolean result.",
            vec![
                var_input("var", "Variable", "ws_connected", "ws_connected"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "websocket_connection_get_phase",
            "WebSocket Connection Get Phase",
            "websocket_connection",
            "Read a WebSocket connection phase.",
            vec![
                var_input("var", "Variable", "ws_phase", "ws_phase"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "websocket_connection_get_subprotocol",
            "WebSocket Connection Get Subprotocol",
            "websocket_connection",
            "Read a negotiated WebSocket subprotocol.",
            vec![
                var_input("var", "Variable", "ws_subprotocol", "ws_subprotocol"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "websocket_connection_send_text",
            "WebSocket Connection Send Text",
            "websocket_connection",
            "Send a WebSocket text message and store the byte count.",
            vec![
                var_input("var", "Variable", "ws_bytes_written", "ws_bytes_written"),
                handle_input("connection", "Connection", "1"),
                text_input("text", "Text", "hello", "hello or $var", true),
            ],
        ),
        flow_value_block(
            "websocket_connection_read_text",
            "WebSocket Connection Read Text",
            "websocket_connection",
            "Read a WebSocket text message.",
            vec![
                var_input("var", "Variable", "ws_text", "ws_text"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "websocket_connection_send_binary_base64",
            "WebSocket Connection Send Binary Base64",
            "websocket_connection",
            "Send a WebSocket binary message from base64 and store the byte count.",
            vec![
                var_input("var", "Variable", "ws_binary_bytes", "ws_binary_bytes"),
                handle_input("connection", "Connection", "1"),
                text_input("payload", "Payload", "aGVsbG8=", "aGVsbG8= or $var", true),
            ],
        ),
        flow_value_block(
            "websocket_connection_read_binary_base64",
            "WebSocket Connection Read Binary Base64",
            "websocket_connection",
            "Read a WebSocket binary message as base64.",
            vec![
                var_input("var", "Variable", "ws_binary", "ws_binary"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "websocket_connection_eof",
            "WebSocket Connection EOF",
            "websocket_connection",
            "Read whether a WebSocket connection reached EOF.",
            vec![
                var_input("var", "Variable", "ws_eof", "ws_eof"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_block(
            "websocket_connection_close",
            "WebSocket Connection Close",
            "websocket_connection",
            "Close a WebSocket connection.",
            vec![
                handle_input("connection", "Connection", "1"),
                number_input("code", "Code", "1000", "1000 or $var", true),
                text_input("reason", "Reason", "done", "done or $var", true),
            ],
        ),
    ]
}

fn webrtc_connection_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "webrtc_connection_new",
            "WebRTC Connection New",
            "webrtc_connection",
            "Create a new WebRTC connection handle.",
            vec![var_input("var", "Variable", "webrtc_conn", "webrtc_conn")],
        ),
        value_block(
            "webrtc_connection_downstream",
            "WebRTC Connection Downstream",
            "webrtc_connection",
            "Get the downstream WebRTC connection handle.",
            vec![var_input(
                "var",
                "Variable",
                "webrtc_downstream",
                "webrtc_downstream",
            )],
        ),
        value_block(
            "webrtc_connection_default_upstream",
            "WebRTC Connection Default Upstream",
            "webrtc_connection",
            "Get the default upstream WebRTC connection handle.",
            vec![var_input(
                "var",
                "Variable",
                "webrtc_default_upstream",
                "webrtc_default_upstream",
            )],
        ),
        flow_value_block(
            "webrtc_connection_is_present",
            "WebRTC Connection Is Present",
            "webrtc_connection",
            "Check whether a WebRTC connection handle is present.",
            vec![
                var_input("var", "Variable", "webrtc_present", "webrtc_present"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_block(
            "webrtc_connection_set_ice_servers",
            "WebRTC Connection Set ICE Servers",
            "webrtc_connection",
            "Set WebRTC ICE server URLs.",
            vec![
                handle_input("connection", "Connection", "1"),
                text_input(
                    "urls",
                    "URLs",
                    "stun:stun.l.google.com:19302",
                    "stun:... or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "webrtc_connection_set_data_channel_label",
            "WebRTC Connection Set Data Channel Label",
            "webrtc_connection",
            "Set the WebRTC data channel label.",
            vec![
                handle_input("connection", "Connection", "1"),
                text_input("label", "Label", "data", "data or $var", true),
            ],
        ),
        flow_block(
            "webrtc_connection_set_remote_description",
            "WebRTC Connection Set Remote Description",
            "webrtc_connection",
            "Set the remote WebRTC session description JSON.",
            vec![
                handle_input("connection", "Connection", "1"),
                text_input(
                    "description_json",
                    "Description JSON",
                    "{\"type\":\"offer\",\"sdp\":\"...\"}",
                    "{\"type\":\"offer\"} or $var",
                    true,
                ),
            ],
        ),
        flow_value_block(
            "webrtc_connection_create_offer",
            "WebRTC Connection Create Offer",
            "webrtc_connection",
            "Create a WebRTC offer and store the description JSON.",
            vec![
                var_input("var", "Variable", "webrtc_offer", "webrtc_offer"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "webrtc_connection_create_answer",
            "WebRTC Connection Create Answer",
            "webrtc_connection",
            "Create a WebRTC answer and store the description JSON.",
            vec![
                var_input("var", "Variable", "webrtc_answer", "webrtc_answer"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "webrtc_connection_connect",
            "WebRTC Connection Connect",
            "webrtc_connection",
            "Connect a WebRTC data channel and store the boolean result.",
            vec![
                var_input("var", "Variable", "webrtc_connected", "webrtc_connected"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "webrtc_connection_get_phase",
            "WebRTC Connection Get Phase",
            "webrtc_connection",
            "Read a WebRTC connection phase.",
            vec![
                var_input("var", "Variable", "webrtc_phase", "webrtc_phase"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "webrtc_connection_send_text",
            "WebRTC Connection Send Text",
            "webrtc_connection",
            "Send a WebRTC data channel text message and store the byte count.",
            vec![
                var_input(
                    "var",
                    "Variable",
                    "webrtc_bytes_written",
                    "webrtc_bytes_written",
                ),
                handle_input("connection", "Connection", "1"),
                text_input("text", "Text", "hello", "hello or $var", true),
            ],
        ),
        flow_value_block(
            "webrtc_connection_read_text",
            "WebRTC Connection Read Text",
            "webrtc_connection",
            "Read a WebRTC data channel text message.",
            vec![
                var_input("var", "Variable", "webrtc_text", "webrtc_text"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "webrtc_connection_send_binary_base64",
            "WebRTC Connection Send Binary Base64",
            "webrtc_connection",
            "Send a WebRTC data channel binary payload from base64 and store the byte count.",
            vec![
                var_input(
                    "var",
                    "Variable",
                    "webrtc_binary_bytes",
                    "webrtc_binary_bytes",
                ),
                handle_input("connection", "Connection", "1"),
                text_input("payload", "Payload", "aGVsbG8=", "aGVsbG8= or $var", true),
            ],
        ),
        flow_value_block(
            "webrtc_connection_read_binary_base64",
            "WebRTC Connection Read Binary Base64",
            "webrtc_connection",
            "Read a WebRTC data channel binary payload as base64.",
            vec![
                var_input("var", "Variable", "webrtc_binary", "webrtc_binary"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "webrtc_connection_eof",
            "WebRTC Connection EOF",
            "webrtc_connection",
            "Read whether a WebRTC data channel reached EOF.",
            vec![
                var_input("var", "Variable", "webrtc_eof", "webrtc_eof"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_block(
            "webrtc_connection_close",
            "WebRTC Connection Close",
            "webrtc_connection",
            "Close a WebRTC connection.",
            vec![handle_input("connection", "Connection", "1")],
        ),
    ]
}

fn udp_socket_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "udp_socket_new",
            "UDP Socket New",
            "udp_socket",
            "Create a new UDP socket handle.",
            vec![var_input("var", "Variable", "udp_socket", "udp_socket")],
        ),
        value_block(
            "udp_socket_downstream",
            "UDP Socket Downstream",
            "udp_socket",
            "Get the downstream UDP socket handle.",
            vec![var_input(
                "var",
                "Variable",
                "udp_downstream",
                "udp_downstream",
            )],
        ),
        value_block(
            "udp_socket_default_upstream",
            "UDP Socket Default Upstream",
            "udp_socket",
            "Get the default upstream UDP socket handle.",
            vec![var_input(
                "var",
                "Variable",
                "udp_default_upstream",
                "udp_default_upstream",
            )],
        ),
        flow_value_block(
            "udp_socket_is_present",
            "UDP Socket Is Present",
            "udp_socket",
            "Check whether a UDP socket handle is present.",
            vec![
                var_input("var", "Variable", "udp_present", "udp_present"),
                handle_input("socket", "Socket", "1"),
            ],
        ),
        flow_block(
            "udp_socket_bind",
            "UDP Socket Bind",
            "udp_socket",
            "Bind a UDP socket to a local address.",
            vec![
                handle_input("socket", "Socket", "1"),
                text_input(
                    "local_addr",
                    "Local Addr",
                    "127.0.0.1:0",
                    "127.0.0.1:0 or $var",
                    true,
                ),
            ],
        ),
        flow_block(
            "udp_socket_set_target",
            "UDP Socket Set Target",
            "udp_socket",
            "Set a UDP socket target address.",
            vec![
                handle_input("socket", "Socket", "1"),
                text_input(
                    "target",
                    "Target",
                    "127.0.0.1:8080",
                    "127.0.0.1:8080 or $var",
                    true,
                ),
            ],
        ),
        flow_value_block(
            "udp_socket_connect",
            "UDP Socket Connect",
            "udp_socket",
            "Connect a UDP socket and store the boolean result.",
            vec![
                var_input("var", "Variable", "udp_connected", "udp_connected"),
                handle_input("socket", "Socket", "1"),
            ],
        ),
        flow_value_block(
            "udp_socket_get_phase",
            "UDP Socket Get Phase",
            "udp_socket",
            "Read a UDP socket phase.",
            vec![
                var_input("var", "Variable", "udp_phase", "udp_phase"),
                handle_input("socket", "Socket", "1"),
            ],
        ),
        flow_value_block(
            "udp_socket_get_local_addr",
            "UDP Socket Get Local Addr",
            "udp_socket",
            "Read a UDP socket local address.",
            vec![
                var_input("var", "Variable", "udp_local_addr", "udp_local_addr"),
                handle_input("socket", "Socket", "1"),
            ],
        ),
        flow_value_block(
            "udp_socket_get_peer_addr",
            "UDP Socket Get Peer Addr",
            "udp_socket",
            "Read a UDP socket peer address.",
            vec![
                var_input("var", "Variable", "udp_peer_addr", "udp_peer_addr"),
                handle_input("socket", "Socket", "1"),
            ],
        ),
        flow_value_block(
            "udp_socket_send_text",
            "UDP Socket Send Text",
            "udp_socket",
            "Send a UDP text payload and store the byte count.",
            vec![
                var_input("var", "Variable", "udp_text_bytes", "udp_text_bytes"),
                handle_input("socket", "Socket", "1"),
                text_input("text", "Text", "hello", "hello or $var", true),
            ],
        ),
        flow_value_block(
            "udp_socket_recv_text",
            "UDP Socket Recv Text",
            "udp_socket",
            "Receive a UDP text payload.",
            vec![
                var_input("var", "Variable", "udp_text", "udp_text"),
                handle_input("socket", "Socket", "1"),
                number_input("max_bytes", "Max Bytes", "1024", "1024 or $var", true),
            ],
        ),
        flow_value_block(
            "udp_socket_send_binary_base64",
            "UDP Socket Send Binary Base64",
            "udp_socket",
            "Send a UDP binary payload from base64 and store the byte count.",
            vec![
                var_input("var", "Variable", "udp_binary_bytes", "udp_binary_bytes"),
                handle_input("socket", "Socket", "1"),
                text_input("payload", "Payload", "aGVsbG8=", "aGVsbG8= or $var", true),
            ],
        ),
        flow_value_block(
            "udp_socket_recv_binary_base64",
            "UDP Socket Recv Binary Base64",
            "udp_socket",
            "Receive a UDP binary payload as base64.",
            vec![
                var_input("var", "Variable", "udp_binary", "udp_binary"),
                handle_input("socket", "Socket", "1"),
                number_input("max_bytes", "Max Bytes", "1024", "1024 or $var", true),
            ],
        ),
        flow_block(
            "udp_socket_close",
            "UDP Socket Close",
            "udp_socket",
            "Close a UDP socket.",
            vec![handle_input("socket", "Socket", "1")],
        ),
    ]
}

fn proxy_stream_blocks() -> Vec<UiBlockDefinition> {
    vec![
        value_block(
            "proxy_stream_downstream",
            "Proxy Stream Downstream",
            "proxy_stream",
            "Get the downstream proxy stream handle.",
            vec![var_input(
                "var",
                "Variable",
                "proxy_downstream",
                "proxy_downstream",
            )],
        ),
        value_block(
            "proxy_stream_exchange",
            "Proxy Stream Exchange",
            "proxy_stream",
            "Project an HTTP exchange as a proxy stream.",
            vec![
                var_input(
                    "var",
                    "Variable",
                    "proxy_exchange_stream",
                    "proxy_exchange_stream",
                ),
                handle_input("exchange", "Exchange", "1"),
            ],
        ),
        value_block(
            "proxy_stream_from_tcp",
            "Proxy Stream From TCP",
            "proxy_stream",
            "Project a TCP stream as a proxy stream.",
            vec![
                var_input("var", "Variable", "proxy_tcp_stream", "proxy_tcp_stream"),
                handle_input("stream", "Stream", "1"),
            ],
        ),
        value_block(
            "proxy_stream_from_tls_plaintext",
            "Proxy Stream From TLS Plaintext",
            "proxy_stream",
            "Project a plaintext TLS session as a proxy stream.",
            vec![
                var_input("var", "Variable", "proxy_tls_stream", "proxy_tls_stream"),
                handle_input("session", "Session", "1"),
            ],
        ),
        value_block(
            "proxy_stream_from_websocket_binary",
            "Proxy Stream From WebSocket Binary",
            "proxy_stream",
            "Project a WebSocket binary stream as a proxy stream.",
            vec![
                var_input("var", "Variable", "proxy_ws_stream", "proxy_ws_stream"),
                handle_input("connection", "Connection", "1"),
            ],
        ),
        flow_value_block(
            "proxy_pipe",
            "Proxy Pipe",
            "proxy",
            "Pipe bytes from one proxy stream to another and store the result string.",
            vec![
                var_input("var", "Variable", "proxy_pipe_result", "proxy_pipe_result"),
                handle_input("source", "Source", "1"),
                handle_input("destination", "Destination", "2"),
                number_input("max_bytes", "Max Bytes", "65536", "65536 or $var", true),
            ],
        ),
        flow_value_block(
            "proxy_bridge",
            "Proxy Bridge",
            "proxy",
            "Bridge two proxy streams and store the result string.",
            vec![
                var_input(
                    "var",
                    "Variable",
                    "proxy_bridge_result",
                    "proxy_bridge_result",
                ),
                handle_input("left", "Left", "1"),
                handle_input("right", "Right", "2"),
                number_input("max_bytes", "Max Bytes", "65536", "65536 or $var", true),
            ],
        ),
    ]
}

fn value_block(
    id: &'static str,
    title: &'static str,
    category: &'static str,
    description: &'static str,
    inputs: Vec<UiBlockInput>,
) -> UiBlockDefinition {
    UiBlockDefinition {
        id,
        title,
        category,
        description,
        inputs,
        outputs: vec![UiBlockOutput {
            key: "value",
            label: "value",
            expr_from_input: Some("var"),
        }],
        accepts_flow: false,
    }
}

fn flow_block(
    id: &'static str,
    title: &'static str,
    category: &'static str,
    description: &'static str,
    inputs: Vec<UiBlockInput>,
) -> UiBlockDefinition {
    UiBlockDefinition {
        id,
        title,
        category,
        description,
        inputs,
        outputs: vec![UiBlockOutput {
            key: "next",
            label: "next",
            expr_from_input: None,
        }],
        accepts_flow: true,
    }
}

fn flow_value_block(
    id: &'static str,
    title: &'static str,
    category: &'static str,
    description: &'static str,
    inputs: Vec<UiBlockInput>,
) -> UiBlockDefinition {
    UiBlockDefinition {
        id,
        title,
        category,
        description,
        inputs,
        outputs: vec![
            UiBlockOutput {
                key: "value",
                label: "value",
                expr_from_input: Some("var"),
            },
            UiBlockOutput {
                key: "next",
                label: "next",
                expr_from_input: None,
            },
        ],
        accepts_flow: true,
    }
}

fn var_input(
    key: &'static str,
    label: &'static str,
    default_value: &'static str,
    placeholder: &'static str,
) -> UiBlockInput {
    UiBlockInput {
        key,
        label,
        input_type: UiInputType::Text,
        default_value,
        placeholder,
        connectable: false,
    }
}

fn text_input(
    key: &'static str,
    label: &'static str,
    default_value: &'static str,
    placeholder: &'static str,
    connectable: bool,
) -> UiBlockInput {
    UiBlockInput {
        key,
        label,
        input_type: UiInputType::Text,
        default_value,
        placeholder,
        connectable,
    }
}

fn bool_input(
    key: &'static str,
    label: &'static str,
    default_value: &'static str,
    placeholder: &'static str,
    connectable: bool,
) -> UiBlockInput {
    UiBlockInput {
        key,
        label,
        input_type: UiInputType::Text,
        default_value,
        placeholder,
        connectable,
    }
}

fn number_input(
    key: &'static str,
    label: &'static str,
    default_value: &'static str,
    placeholder: &'static str,
    connectable: bool,
) -> UiBlockInput {
    UiBlockInput {
        key,
        label,
        input_type: UiInputType::Number,
        default_value,
        placeholder,
        connectable,
    }
}

fn handle_input(
    key: &'static str,
    label: &'static str,
    default_value: &'static str,
) -> UiBlockInput {
    UiBlockInput {
        key,
        label,
        input_type: UiInputType::Number,
        default_value,
        placeholder: "1 or $var",
        connectable: true,
    }
}
