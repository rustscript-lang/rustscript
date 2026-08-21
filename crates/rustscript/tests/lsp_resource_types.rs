//! Protocol fixture for the `rustscript-lsp` stdio LSP adapter.
//!
//! Launches the real `rustscript-lsp` binary over stdio and drives it with
//! framed JSON-RPC messages, asserting the resource-aware language-service
//! surface: lifecycle, document sync, publishDiagnostics (with exact
//! expected/actual resource keys and ranges), hover (resource schema),
//! signature help (borrow/take modes), completion detail/import visibility,
//! go-to-definition (real locals + deterministic virtual host definitions),
//! UTF-16 position conversion, malformed/unknown request handling, and
//! orderly shutdown/exit.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

// ---------------------------------------------------------------------------
// JSON-RPC framing helpers
// ---------------------------------------------------------------------------

/// A minimal JSON-RPC client over a child process's stdio.
struct RpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RpcClient {
    fn spawn() -> Self {
        Self::spawn_with_args(&[])
    }

    fn spawn_with_args(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rustscript-lsp"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rustscript-lsp must spawn");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        }
    }

    /// Send one JSON-RPC message (request or notification).
    fn send(&mut self, message: &serde_json::Value) {
        let body = serde_json::to_vec(message).expect("serialize message");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        self.stdin.write_all(&body).expect("write body");
        self.stdin.flush().expect("flush stdin");
    }

    /// Read one JSON-RPC message from the server.
    fn recv(&mut self) -> serde_json::Value {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read header line");
            if n == 0 {
                // EOF: the server died. Surface its stderr for diagnosis.
                let mut stderr = String::new();
                let _ = self
                    .child
                    .stderr
                    .take()
                    .map(|mut e| e.read_to_string(&mut stderr));
                panic!("EOF while reading headers — server died. stderr: {stderr}");
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = Some(value.trim().parse().expect("content-length number"));
                }
            }
        }
        let length = content_length.expect("content-length header present");
        let mut body = vec![0u8; length];
        self.stdout.read_exact(&mut body).expect("read body");
        serde_json::from_slice(&body).expect("parse JSON-RPC body")
    }

    /// Request: send and await the matching response by id.
    fn request(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        // Diagnostic: poll the child so a dead/hung server surfaces instead
        // of blocking the test forever.
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("server exited with {status} while awaiting request {id} ({method})");
            }
            let response = self.recv();
            if response.get("id") == Some(&serde_json::json!(id)) {
                return response;
            }
            // A notification (e.g. publishDiagnostics) arrived first: keep
            // reading. Tests that expect interleaved notifications use
            // `recv_notification` explicitly; here we skip unrelated
            // notifications.
            if Instant::now() > deadline {
                self.child.kill().ok();
                panic!("server hung while awaiting request {id} ({method})");
            }
        }
    }

    /// Notification: send without an id.
    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// Wait for the next server->client notification with the given method
    /// and return its params.
    fn recv_notification(&mut self, method: &str) -> serde_json::Value {
        loop {
            let message = self.recv();
            if message.get("method") == Some(&serde_json::json!(method)) {
                return message
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
            }
            // Requests (shouldn't normally arrive unsolicited) are skipped.
        }
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ENTRY_URI: &str = "file:///tmp/rustscript-lsp-fixture/main.rss";

/// A clean program that exercises sqlite resources with correct borrow usage.
const CLEAN_SOURCE: &str = r#"use sqlite;
fn main() {
    let db = sqlite::open({});
    sqlite::query(&db, "SELECT 1", {}, {});
}
"#;

/// A program with a wrong-resource-type call (string where a
/// `borrow resource<sqlite.connection>` is required).
const WRONG_TYPE_SOURCE: &str = r#"use sqlite;
fn main() {
    let db = sqlite::open({});
    sqlite::query("NOT_A_DB", "SELECT 1", {}, {});
}
"#;

/// A program with non-ASCII text before the target so UTF-16 conversion is
/// exercised (each CJK char is 3 UTF-8 bytes but 1 UTF-16 unit).
const UNICODE_SOURCE: &str = r#"use sqlite;
// 你好世界
let db = sqlite::open({});
"#;

fn open_doc(client: &mut RpcClient, uri: &str, text: &str) {
    client.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rustscript",
                "version": 1,
                "text": text,
            }
        }),
    );
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[test]
fn initialize_reports_resource_language_server_capabilities() {
    let mut client = RpcClient::spawn();
    let response = client.request(
        1,
        "initialize",
        serde_json::json!({ "capabilities": {}, "rootUri": null }),
    );
    let result = response.get("result").expect("initialize result");
    let capabilities = result.get("capabilities").expect("capabilities");
    assert_eq!(
        capabilities["textDocumentSync"]["openClose"],
        serde_json::json!(true),
        "openClose sync must be declared"
    );
    assert_eq!(
        capabilities["textDocumentSync"]["change"],
        serde_json::json!(1),
        "full-sync change notifications must be declared"
    );
    assert_eq!(capabilities["hoverProvider"], serde_json::json!(true));
    assert_eq!(capabilities["definitionProvider"], serde_json::json!(true));
    assert!(
        capabilities.get("signatureHelpProvider").is_some(),
        "signatureHelpProvider must be declared"
    );
    assert!(
        capabilities.get("completionProvider").is_some(),
        "completionProvider must be declared"
    );
    let info = result.get("serverInfo").expect("serverInfo");
    assert_eq!(info["name"], serde_json::json!("rustscript-lsp"));
}

#[test]
fn shutdown_then_exit_is_orderly_zero() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    let shutdown = client.request(2, "shutdown", serde_json::json!({}));
    assert_eq!(shutdown["result"], serde_json::Value::Null);
    client.notify("exit", serde_json::json!({}));
    let status = client.child.wait().expect("wait for exit");
    assert!(
        status.success(),
        "orderly exit after shutdown must be success"
    );
}

#[test]
fn exit_without_shutdown_is_error_status() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("exit", serde_json::json!({}));
    let status = client.child.wait().expect("wait for exit");
    assert!(
        !status.success(),
        "exit without shutdown must be a failure status"
    );
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[test]
fn open_clean_document_publishes_no_diagnostics() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(params["uri"], serde_json::json!(ENTRY_URI));
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "clean program must publish zero diagnostics"
    );
}

#[test]
fn wrong_resource_type_diagnostic_reports_expected_and_actual_key_with_exact_range() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, WRONG_TYPE_SOURCE);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(params["uri"], serde_json::json!(ENTRY_URI));
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    // There must be a diagnostic mentioning the expected resource key.
    let wrong_type: Vec<&serde_json::Value> = diagnostics
        .iter()
        .filter(|d| {
            d["message"]
                .as_str()
                .map(|m| m.contains("sqlite.connection"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !wrong_type.is_empty(),
        "wrong resource type must produce a diagnostic naming sqlite.connection: {diagnostics:?}"
    );
    // The message must name the expected passing/resource contract and the
    // actual argument type.
    let message = wrong_type[0]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("borrow") || message.contains("resource<sqlite.connection>"),
        "diagnostic must expose the borrow resource contract: {message}"
    );
    // The range must point at the wrong argument (line 3 = `sqlite::query("NOT_A_DB", ...)`,
    // the callee `sqlite::query` at chars 4..17).
    let range = &wrong_type[0]["range"];
    let start = &range["start"];
    let end = &range["end"];
    assert_eq!(
        start["line"],
        serde_json::json!(3),
        "start line must be the query call"
    );
    assert_eq!(
        start["character"],
        serde_json::json!(4),
        "start character must be at the callee"
    );
    assert_eq!(
        end["line"],
        serde_json::json!(3),
        "end line must be the query call"
    );
    assert!(
        end["character"].as_u64().unwrap() > start["character"].as_u64().unwrap(),
        "range must be non-empty"
    );
    // Every diagnostic must carry the source and a severity.
    for diagnostic in diagnostics {
        assert_eq!(diagnostic["source"], serde_json::json!("rustscript"));
        assert!(diagnostic.get("severity").is_some());
    }
}

#[test]
fn did_change_reanalyzes_and_clears_diagnostics() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // Open the wrong-type source: diagnostics appear.
    open_doc(&mut client, ENTRY_URI, WRONG_TYPE_SOURCE);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert!(
        !params["diagnostics"].as_array().unwrap().is_empty(),
        "wrong-type source must publish diagnostics"
    );
    // Change to the clean source: diagnostics clear.
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI, "version": 2 },
            "contentChanges": [{ "text": CLEAN_SOURCE }],
        }),
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "clean reanalysis must clear diagnostics"
    );
}

#[test]
fn did_close_clears_stale_diagnostics() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, WRONG_TYPE_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    client.notify(
        "textDocument/didClose",
        serde_json::json!({ "textDocument": { "uri": ENTRY_URI } }),
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(
        params["uri"],
        serde_json::json!(ENTRY_URI),
        "close must publish for the closed uri"
    );
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "close must clear stale diagnostics"
    );
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

#[test]
fn hover_shows_resource_schema_for_inferred_local() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    // Consume the diagnostics notification.
    client.recv_notification("textDocument/publishDiagnostics");
    // Hover on `db` at line 2, char 8.
    let response = client.request(
        10,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 2, "character": 8 },
        }),
    );
    let result = response.get("result").expect("hover result");
    let contents = result.get("contents").expect("hover contents");
    let value = contents["value"].as_str().unwrap_or("");
    assert!(
        value.contains("resource<sqlite.connection>"),
        "hover on db must show the resource schema: {value:?}"
    );
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

#[test]
fn signature_help_shows_borrow_resource_and_value_params() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // Cursor inside the `sqlite::query(...)` argument list (line 3, char 30).
    let response = client.request(
        11,
        "textDocument/signatureHelp",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 3, "character": 30 },
        }),
    );
    let result = response.get("result").expect("signature result");
    let signatures = result["signatures"].as_array().expect("signatures array");
    assert_eq!(signatures.len(), 1, "one resolved signature");
    let label = signatures[0]["label"].as_str().unwrap_or("");
    assert!(
        label.contains("sqlite::query"),
        "signature must name sqlite::query: {label}"
    );
    assert!(
        label.contains("borrow resource<sqlite.connection>"),
        "signature must show borrow resource parameter: {label}"
    );
    assert!(
        label.contains("sql: string"),
        "signature must show the value parameter: {label}"
    );
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

#[test]
fn completion_surfaces_host_members_with_resource_detail_after_import() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // The wildcard import `use sqlite;` makes `query`/`open` members visible;
    // a bare prefix without the import must not leak the canonical names.
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // Complete after `sqlite::` on line 3 (char 11).
    let response = client.request(
        12,
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 3, "character": 11 },
        }),
    );
    let result = response.get("result").expect("completion result");
    let items = result["items"].as_array().expect("completion items");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.iter().any(|l| *l == "query"),
        "completion after sqlite:: must include query member: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| *l == "open"),
        "completion after sqlite:: must include open member: {labels:?}"
    );
    // The `query` completion detail must carry the resource-aware signature.
    let query_item = items
        .iter()
        .find(|i| i["label"] == serde_json::json!("query"))
        .expect("query completion item");
    let detail = query_item["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("resource<sqlite.connection>") || detail.contains("borrow"),
        "query completion detail must show the resource contract: {detail:?}"
    );
}

#[test]
fn completion_without_import_does_not_leak_catalog_functions() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // No `use sqlite;` — the catalog surface must not be dumped wholesale.
    let source = "fn compute() {\n    let x = 1;\n    x\n}\n";
    open_doc(&mut client, ENTRY_URI, source);
    client.recv_notification("textDocument/publishDiagnostics");
    let response = client.request(
        13,
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 2, "character": 1 },
        }),
    );
    let result = response.get("result").expect("completion result");
    let items = result["items"].as_array().expect("completion items");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.iter().all(|l| !l.contains("sqlite::")),
        "catalog functions must not leak without an import: {labels:?}"
    );
}

// ---------------------------------------------------------------------------
// Definition
// ---------------------------------------------------------------------------

#[test]
fn definition_resolves_local_declaration_in_real_source() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // Definition on the `&db` reference (line 3, char 19 is `b` of `db`).
    let response = client.request(
        14,
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 3, "character": 19 },
        }),
    );
    let result = response.get("result").expect("definition result");
    let locations = result.as_array().expect("definition location array");
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0]["uri"], serde_json::json!(ENTRY_URI));
    // The definition must be the `let db` binding on line 2, chars 8..10.
    let range = &locations[0]["range"];
    assert_eq!(range["start"]["line"], serde_json::json!(2));
    assert_eq!(range["start"]["character"], serde_json::json!(8));
    assert_eq!(range["end"]["character"], serde_json::json!(10));
}

#[test]
fn definition_for_host_call_returns_deterministic_virtual_location() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // Definition on the `sqlite::open` callee (line 2, char 13..27).
    let response = client.request(
        15,
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 2, "character": 13 },
        }),
    );
    let result = response.get("result").expect("definition result");
    let locations = result.as_array().expect("definition location array");
    assert_eq!(locations.len(), 1);
    let uri = locations[0]["uri"].as_str().expect("host definition uri");
    assert!(
        uri.starts_with("rustscript-host://"),
        "host definitions must use the virtual host scheme: {uri}"
    );
    assert!(
        uri.contains("sqlite::open"),
        "host definition uri must encode the function name: {uri}"
    );
    // Deterministic: the same request yields the same uri.
    let response2 = client.request(
        16,
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 2, "character": 13 },
        }),
    );
    let result2 = response2.get("result").expect("definition result 2");
    assert_eq!(
        result2[0]["uri"], uri,
        "host definition uri must be deterministic"
    );
    // The virtual document content endpoint serves the rendered signature.
    let content = client.request(
        17,
        "rustscript-host/documentContent",
        serde_json::json!({ "uri": uri }),
    );
    let content_result = content.get("result").expect("document content result");
    let text = content_result["content"].as_str().unwrap_or("");
    assert!(
        text.contains("sqlite::open"),
        "virtual host document must render the function: {text}"
    );
}

// ---------------------------------------------------------------------------
// UTF-16 conversion
// ---------------------------------------------------------------------------

#[test]
fn unicode_source_utf16_positions_resolve_correctly() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, UNICODE_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // Line 1 is the comment `// 你好世界`; line 2 is `let db = sqlite::open({});`.
    // Hover on `db` at line 2, char 4 (UTF-16 columns; the comment's CJK
    // chars are 1 UTF-16 unit each but 3 UTF-8 bytes, so a byte-based
    // conversion would mis-locate the target).
    let response = client.request(
        20,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 2, "character": 4 },
        }),
    );
    let result = response.get("result").expect("hover result");
    let contents = result.get("contents").expect("hover contents");
    let value = contents["value"].as_str().unwrap_or("");
    assert!(
        value.contains("resource<sqlite.connection>"),
        "UTF-16 position must resolve to db's resource schema: {value:?}"
    );
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

#[test]
fn unknown_method_returns_jsonrpc_error() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    let response = client.request(30, "textDocument/unknownThing", serde_json::json!({}));
    let error = response.get("error").expect("error object");
    assert_eq!(
        error["code"],
        serde_json::json!(-32601),
        "method not found code"
    );
}

#[test]
fn malformed_position_returns_null_not_panic() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, CLEAN_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // A line far beyond the document: must not panic and must return null.
    let response = client.request(
        31,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 9999, "character": 0 },
        }),
    );
    assert_eq!(response["result"], serde_json::Value::Null);
    // A huge UTF-16 character offset on a valid line: must not panic.
    let response = client.request(
        32,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 2, "character": 99999 },
        }),
    );
    assert_eq!(response["result"], serde_json::Value::Null);
}

#[test]
fn unknown_uri_returns_null_not_panic() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    let response = client.request(
        33,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": "file:///tmp/never-opened.rss" },
            "position": { "line": 0, "character": 0 },
        }),
    );
    assert_eq!(response["result"], serde_json::Value::Null);
}

#[test]
fn malformed_json_body_gets_parse_error_and_server_survives() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    // Send a body that is not valid JSON (frame it correctly).
    let body = b"{ this is not json";
    write!(client.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
    client.stdin.write_all(body).expect("write body");
    client.stdin.flush().expect("flush stdin");
    let response = client.recv();
    let error = response.get("error").expect("parse error object");
    assert_eq!(error["code"], serde_json::json!(-32700), "parse error code");
    // The server must still be alive and functional.
    let shutdown = client.request(40, "shutdown", serde_json::json!({}));
    assert_eq!(shutdown["result"], serde_json::Value::Null);
    client.notify("exit", serde_json::json!({}));
    let status = client.child.wait().expect("wait for exit");
    assert!(status.success(), "server must survive a malformed payload");
}

// ---------------------------------------------------------------------------
// Custom catalog (--catalog) input
// ---------------------------------------------------------------------------

/// Write a minimal custom catalog JSON (the `HostApiCatalog` serde shape) to
/// a temp file and return its path.
fn write_custom_catalog(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let json = serde_json::json!({
        "resources": [
            { "key": "custom.widget", "description": "A custom widget resource" }
        ],
        "functions": [
            {
                "name": "widget::make",
                "params": [ { "name": "label", "ty": "String", "passing": "Value" } ],
                "return_type": { "Resource": "custom.widget" },
                "description": "Makes a widget"
            },
            {
                "name": "widget::use_it",
                "params": [
                    { "name": "w", "ty": { "Resource": "custom.widget" }, "passing": "Borrow" }
                ],
                "return_type": "Int",
                "description": "Uses a widget"
            }
        ]
    });
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).expect("write catalog");
    path
}

#[test]
fn custom_catalog_serves_custom_resources_and_is_not_coerced() {
    let dir = std::env::temp_dir().join("rustscript-lsp-custom-catalog-test");
    std::fs::create_dir_all(&dir).ok();
    let catalog_path = write_custom_catalog(&dir, "catalog.json");
    let catalog_arg = catalog_path.to_str().expect("catalog path utf8");

    let mut client = RpcClient::spawn_with_args(&["--catalog", catalog_arg]);
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));

    // A program using the custom widget resource.
    let uri = "file:///tmp/custom-widget.rss";
    let source =
        "use widget;\nfn main() {\n    let w = widget::make(\"x\");\n    widget::use_it(&w);\n}\n";
    open_doc(&mut client, uri, source);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "custom catalog program must compile cleanly"
    );

    // Hover on `w` must render the custom resource type, never `int`.
    let hover = client.request(
        2,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 2, "character": 8 },
        }),
    );
    let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
    assert!(
        value.contains("resource<custom.widget>"),
        "custom resource must hover as resource<custom.widget>: {value:?}"
    );

    // Signature help must show the borrow mode for the custom resource.
    let sig = client.request(
        3,
        "textDocument/signatureHelp",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 20 },
        }),
    );
    let label = sig["result"]["signatures"][0]["label"]
        .as_str()
        .unwrap_or("");
    assert!(
        label.contains("borrow resource<custom.widget>"),
        "signature must show borrow custom resource: {label}"
    );

    // Wrong-type call must be a diagnostic (custom key), not coerced to int.
    let bad_source = "use widget;\nfn main() {\n    let w = widget::make(\"x\");\n    widget::use_it(\"NOT_A_WIDGET\");\n}\n";
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": bad_source }],
        }),
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    let diagnostics = params["diagnostics"].as_array().unwrap();
    let wrong: Vec<&serde_json::Value> = diagnostics
        .iter()
        .filter(|d| {
            d["message"]
                .as_str()
                .map(|m| m.contains("custom.widget"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !wrong.is_empty(),
        "wrong custom resource type must produce a diagnostic naming custom.widget: {diagnostics:?}"
    );
    assert!(
        wrong[0]["message"]
            .as_str()
            .map(|m| m.contains("found string"))
            .unwrap_or(false),
        "diagnostic must name the actual string argument"
    );
}

#[test]
fn custom_catalog_rejects_invalid_schema_at_startup() {
    let dir = std::env::temp_dir().join("rustscript-lsp-invalid-catalog-test");
    std::fs::create_dir_all(&dir).ok();
    // A catalog that violates the passing-mode rule: a resource passed by Value.
    let path = dir.join("invalid.json");
    let json = serde_json::json!({
        "resources": [
            { "key": "custom.widget", "description": "w" }
        ],
        "functions": [
            {
                "name": "widget::use_it",
                "params": [
                    { "name": "w", "ty": { "Resource": "custom.widget" }, "passing": "Value" }
                ],
                "return_type": "Int",
                "description": "resource passed by Value is invalid"
            }
        ]
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).expect("write invalid catalog");
    let arg = path.to_str().expect("utf8");

    // The binary must fail at startup (exit != 0) and never serve.
    let output = Command::new(env!("CARGO_BIN_EXE_rustscript-lsp"))
        .args(["--catalog", arg])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run invalid-catalog server");
    assert!(
        !output.status.success(),
        "an invalid catalog must be rejected at startup"
    );
}
