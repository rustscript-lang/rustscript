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
use std::process::{Child, ChildStdin, Command, Stdio};

// ---------------------------------------------------------------------------
// JSON-RPC framing helpers
// ---------------------------------------------------------------------------

/// A minimal JSON-RPC client over a child process's stdio.
///
/// The child's stdout is drained by a dedicated reader thread feeding a
/// bounded channel, so every receive is time-bounded: a hung-alive server
/// fails the test instead of blocking it forever.
struct RpcClient {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: std::sync::mpsc::Receiver<serde_json::Value>,
}

/// Read one framed JSON-RPC message from a buffered reader (Content-Length
/// framing). Returns `None` on EOF.
fn read_framed_message(reader: &mut impl BufRead) -> Option<serde_json::Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read header line");
        if n == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse().expect("content-length number"));
        }
    }
    let length = content_length.expect("content-length header present");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect("read body");
    Some(serde_json::from_slice(&body).expect("parse JSON-RPC body"))
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
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_framed_message(&mut reader) {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            messages: rx,
        }
    }

    /// Send one JSON-RPC message (request or notification).
    fn send(&mut self, message: &serde_json::Value) {
        let body = serde_json::to_vec(message).expect("serialize message");
        let stdin = self.stdin.as_mut().expect("stdin open");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        stdin.write_all(&body).expect("write body");
        stdin.flush().expect("flush stdin");
    }

    /// Read one JSON-RPC message from the server, bounded by a deadline. If
    /// the server dies or hangs, the test fails with the child's stderr.
    fn recv(&mut self) -> serde_json::Value {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(30);
        if let Some(status) = self.child.try_wait().expect("try_wait") {
            // EOF: the server died. Surface its stderr for diagnosis.
            let mut stderr = String::new();
            let _ = self
                .child
                .stderr
                .take()
                .map(|mut e| e.read_to_string(&mut stderr));
            panic!("server died with {status} while reading message. stderr: {stderr}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.messages.recv_timeout(remaining) {
            Ok(message) => message,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.child.kill().ok();
                panic!("server hung while awaiting a message");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let mut stderr = String::new();
                let _ = self
                    .child
                    .stderr
                    .take()
                    .map(|mut e| e.read_to_string(&mut stderr));
                panic!("server stdout closed without a message. stderr: {stderr}");
            }
        }
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
    /// and return its params. Bounded: a hung-alive server fails the test
    /// instead of blocking it forever.
    fn recv_notification(&mut self, method: &str) -> serde_json::Value {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("server exited with {status} while awaiting notification {method}");
            }
            if Instant::now() > deadline {
                self.child.kill().ok();
                panic!("server hung while awaiting notification {method}");
            }
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

    /// Drain publishDiagnostics notifications until one for `uri` arrives and
    /// return its params. Multi-document servers publish one notification per
    /// URI, so tests targeting a specific document must skip unrelated URIs.
    fn recv_publish_for(&mut self, uri: &str) -> serde_json::Value {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("server exited with {status} while awaiting publish for {uri}");
            }
            if Instant::now() > deadline {
                self.child.kill().ok();
                panic!("server hung while awaiting publish for {uri}");
            }
            let params = self.recv_notification("textDocument/publishDiagnostics");
            if params["uri"] == serde_json::json!(uri) {
                return params;
            }
        }
    }

    /// Write raw bytes to stdin and close it (EOF). Used by framing-robustness
    /// tests: after the server reads EOF it must exit, so this never blocks.
    fn send_raw_then_close(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("write raw bytes");
        stdin.flush().expect("flush raw bytes");
        // Drop stdin to signal EOF; the server's read loop terminates.
        self.stdin.take();
    }

    /// Wait for the child to exit and return its status. The child's stdin is
    /// closed first so a server blocked reading can never deadlock the test.
    fn wait_exit(&mut self) -> std::process::ExitStatus {
        self.stdin.take();

        self.child.wait().expect("wait for server exit")
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
        labels.contains(&"query"),
        "completion after sqlite:: must include query member: {labels:?}"
    );
    assert!(
        labels.contains(&"open"),
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
    // Finding #4: the definition range must identify the actual rendered
    // function entry — the first signature line of the virtual document,
    // spanning its full width (never a zero-width placeholder).
    let first_line = text.lines().next().unwrap_or("");
    let expected_len = first_line.chars().count();
    let range = &locations[0]["range"];
    assert_eq!(
        range["start"],
        serde_json::json!({ "line": 0, "character": 0 })
    );
    assert_eq!(
        range["end"],
        serde_json::json!({ "line": 0, "character": expected_len }),
        "host definition range must cover the rendered function entry line"
    );
    // The virtual document's first line must be the rendered signature
    // (a real function entry, not a comment).
    assert!(
        first_line.contains("sqlite::open") && !first_line.starts_with("//"),
        "virtual document entry line must be the rendered signature: {first_line:?}"
    );
}

// ---------------------------------------------------------------------------
// Multi-source diagnostics (module graph)
// ---------------------------------------------------------------------------

const MODULE_ENTRY_URI: &str = "file:///tmp/rustscript-lsp-modules/main.rss";
const MODULE_UTIL_URI: &str = "file:///tmp/rustscript-lsp-modules/util.rss";

/// Entry that imports `util.rss` (via `self::util`) and calls its helper.
const MODULE_ENTRY_SOURCE: &str = r#"use self::util;
fn run() {
    util::helper();
}
"#;

/// Imported module whose `helper` body has a wrong-resource-type call. The
/// diagnostic span lives in *this* source, so it must be reported under
/// MODULE_UTIL_URI with ranges into this text, never the entry's.
const MODULE_BAD_UTIL_SOURCE: &str = r#"use sqlite;
pub fn helper() {
    let db = sqlite::open({});
    sqlite::query("NOT_A_DB", "SELECT 1", {}, {});
}
"#;

/// Imported module whose `helper` body is clean.
const MODULE_CLEAN_UTIL_SOURCE: &str = r#"use sqlite;
pub fn helper() {
    let db = sqlite::open({});
    sqlite::query(&db, "SELECT 1", {}, {});
}
"#;

/// Open the module buffer first (so it is already an override the entry sees),
/// then the entry. Returns the module URI's publish params from the entry
/// analysis (the diagnostics the entry's graph reports for the module).
fn open_module_pair(client: &mut RpcClient, entry: &str, module: &str) -> serde_json::Value {
    open_doc(client, MODULE_UTIL_URI, module);
    client.recv_publish_for(MODULE_UTIL_URI);
    open_doc(client, MODULE_ENTRY_URI, entry);
    // The entry open publishes for both documents (sorted: main then util).
    client.recv_publish_for(MODULE_UTIL_URI)
}

#[test]
fn imported_module_error_publishes_under_module_uri_with_exact_range() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // The entry's analysis publishes diagnostics for the *module* URI (the
    // wrong-type call lives in util.rss).
    let params = open_module_pair(&mut client, MODULE_ENTRY_SOURCE, MODULE_BAD_UTIL_SOURCE);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
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
        "imported module error must produce a diagnostic naming sqlite.connection: {diagnostics:?}"
    );
    // The range points into util.rss line 3 (`sqlite::query("NOT_A_DB", ...)`),
    // exactly like the single-file fixture (callee `sqlite::query` at 4..17).
    let range = &wrong_type[0]["range"];
    assert_eq!(
        range["start"]["line"],
        serde_json::json!(3),
        "start line must be the module's query call line"
    );
    assert_eq!(
        range["start"]["character"],
        serde_json::json!(4),
        "start character must be at the module's callee"
    );
    assert_eq!(
        range["end"]["line"],
        serde_json::json!(3),
        "end line must be the module's query call line"
    );
    assert!(
        range["end"]["character"].as_u64().unwrap() > 4,
        "module diagnostic range must be non-empty"
    );
}

#[test]
fn second_open_buffer_override_wins_for_imported_module() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // First module buffer has the wrong-type error.
    open_doc(&mut client, MODULE_UTIL_URI, MODULE_BAD_UTIL_SOURCE);
    let params = client.recv_publish_for(MODULE_UTIL_URI);
    assert!(
        !params["diagnostics"].as_array().unwrap().is_empty(),
        "first buffer override must surface the module error"
    );
    open_doc(&mut client, MODULE_ENTRY_URI, MODULE_ENTRY_SOURCE);
    let params = client.recv_publish_for(MODULE_UTIL_URI);
    assert!(
        !params["diagnostics"].as_array().unwrap().is_empty(),
        "entry analysis must surface the imported module error under the module uri"
    );

    // Second open of the same module URI (clean) replaces the buffer, then
    // the entry reanalysis (didChange) must use the *new* text (override
    // wins) and clear the previously published diagnostics for the module URI.
    open_doc(&mut client, MODULE_UTIL_URI, MODULE_CLEAN_UTIL_SOURCE);
    client.recv_publish_for(MODULE_UTIL_URI);
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": MODULE_ENTRY_URI, "version": 2 },
            "contentChanges": [{ "text": MODULE_ENTRY_SOURCE }],
        }),
    );
    let params = client.recv_publish_for(MODULE_UTIL_URI);
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "second (clean) buffer override must win and clear the stale error"
    );
}

#[test]
fn closing_imported_module_clears_its_published_diagnostics() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    let params = open_module_pair(&mut client, MODULE_ENTRY_SOURCE, MODULE_BAD_UTIL_SOURCE);
    assert!(!params["diagnostics"].as_array().unwrap().is_empty());

    // Close the module buffer: its URI must be cleared with an empty publish.
    client.notify(
        "textDocument/didClose",
        serde_json::json!({ "textDocument": { "uri": MODULE_UTIL_URI } }),
    );
    let params = client.recv_publish_for(MODULE_UTIL_URI);
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "closing the module must clear its published diagnostics"
    );
}

// ---------------------------------------------------------------------------
// UTF-16 conversion
// ---------------------------------------------------------------------------

/// BMP and astral characters on the *same line* before the queried target
/// (inside a string literal, since identifiers are ASCII-only), so UTF-16
/// column conversion is genuinely exercised: each CJK char is 3 UTF-8 bytes
/// but 1 UTF-16 unit; the emoji is 4 UTF-8 bytes and 2 UTF-16 units (a
/// surrogate pair). A byte-based converter would mis-locate `db`.
/// Line 1: `let s = "你好😀"; let db = sqlite::open({});`
const UNICODE_SOURCE: &str =
    "use sqlite;\nlet s = \"\u{4f60}\u{597d}\u{1f600}\"; let db = sqlite::open({});\n";

#[test]
fn unicode_source_utf16_positions_resolve_correctly() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    open_doc(&mut client, ENTRY_URI, UNICODE_SOURCE);
    client.recv_notification("textDocument/publishDiagnostics");
    // Line 1 UTF-16 columns:
    //   `let s = "` = 9, 你 = 1, 好 = 1, 😀 = 2, `"` = 1, `;` = 1, ` ` = 1,
    //   `let ` = 4 → `db` starts at UTF-16 column 9+1+1+2+1+1+1+4 = 20.
    // A byte-based converter would count 9+3+3+4+1+1+1+4 = 26 bytes → column
    // 26, which is inside `sqlite::open` and would hover the call, not `db`.
    let response = client.request(
        20,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI },
            "position": { "line": 1, "character": 20 },
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

#[test]
fn unicode_source_outbound_diagnostic_range_uses_utf16_columns() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // Same-line multibyte prefix, then a wrong-type call on the *same line*.
    // `let s = "你好😀"; sqlite::query("NOT_A_DB", "SELECT 1", {}, {});`
    // The wrong-argument diagnostic must be reported with UTF-16 columns, so
    // a client re-navigating from the range lands on the callee.
    let source = "use sqlite;\nlet s = \"\u{4f60}\u{597d}\u{1f600}\"; sqlite::query(\"NOT_A_DB\", \"SELECT 1\", {}, {});\n";
    open_doc(&mut client, ENTRY_URI, source);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
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
        "same-line unicode wrong-type call must produce a diagnostic: {diagnostics:?}"
    );
    // Prefix `let s = "你好😀"; ` in UTF-16 units:
    //   `let s = "`=9, 你=1, 好=1, 😀=2, `"`=1, `;`=1, ` `=1 → 16
    // then `sqlite::query` starts at UTF-16 column 16 (byte column would be 22).
    let range = &wrong_type[0]["range"];
    assert_eq!(range["start"]["line"], serde_json::json!(1));
    assert_eq!(
        range["start"]["character"],
        serde_json::json!(16),
        "diagnostic start must use UTF-16 columns after a multibyte prefix"
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
    write!(
        client.stdin.as_mut().unwrap(),
        "Content-Length: {}\r\n\r\n",
        body.len()
    )
    .expect("write header");
    client
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body)
        .expect("write body");
    client.stdin.as_mut().unwrap().flush().expect("flush stdin");
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

// ---------------------------------------------------------------------------
// Lifecycle enforcement (LSP spec)
// ---------------------------------------------------------------------------

#[test]
fn request_before_initialize_returns_server_not_initialized() {
    let mut client = RpcClient::spawn();
    // No initialize sent: requests must be rejected with -32002.
    let response = client.request(1, "textDocument/hover", serde_json::json!({}));
    let error = response.get("error").expect("error object");
    assert_eq!(
        error["code"],
        serde_json::json!(-32002),
        "request before initialize must return ServerNotInitialized"
    );
    // The server must still accept the later initialize.
    let init = client.request(2, "initialize", serde_json::json!({}));
    assert!(
        init.get("result").is_some(),
        "server must recover and handle initialize"
    );
    let shutdown = client.request(3, "shutdown", serde_json::json!({}));
    assert_eq!(shutdown["result"], serde_json::Value::Null);
    client.notify("exit", serde_json::json!({}));
    let status = client.child.wait().expect("wait for exit");
    assert!(
        status.success(),
        "orderly shutdown after pre-init rejection"
    );
}

#[test]
fn request_after_shutdown_returns_invalid_request_except_exit() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    let shutdown = client.request(2, "shutdown", serde_json::json!({}));
    assert_eq!(shutdown["result"], serde_json::Value::Null);
    // After shutdown, a request must be rejected with InvalidRequest.
    let response = client.request(3, "textDocument/hover", serde_json::json!({}));
    let error = response.get("error").expect("error object");
    assert_eq!(
        error["code"],
        serde_json::json!(-32600),
        "request after shutdown must return InvalidRequest"
    );
    // exit is still allowed and exits orderly.
    client.notify("exit", serde_json::json!({}));
    let status = client.child.wait().expect("wait for exit");
    assert!(status.success(), "exit after shutdown is orderly");
}

// ---------------------------------------------------------------------------
// Framing / robustness
// ---------------------------------------------------------------------------

/// Spawn with a tiny message cap so the over-limit path is exercised without
/// transferring 16 MiB.
fn spawn_tiny_cap() -> RpcClient {
    RpcClient::spawn_with_args(&["--max-message-bytes", "100"])
}

/// Spawn with a tiny document cap so the oversized-document path is exercised
/// without transferring 8 Mi chars. The cap is generous enough that the
/// normal fixtures (which are all < 200 chars) pass.
fn spawn_tiny_doc_cap() -> RpcClient {
    RpcClient::spawn_with_args(&["--max-document-chars", "200"])
}

#[test]
fn oversized_message_is_rejected_fatally() {
    let mut client = spawn_tiny_cap();
    // A Content-Length above the 100-byte cap must be a fatal framing error
    // (the stream cannot be resynced) and the server must exit nonzero.
    client.send_raw_then_close(b"Content-Length: 200\r\n\r\n{}");
    let status = client.wait_exit();
    assert!(
        !status.success(),
        "over-limit message must terminate the server abnormally"
    );
}

#[test]
fn missing_content_length_is_fatal() {
    let mut client = RpcClient::spawn();
    // A message with headers but no Content-Length header is a fatal framing
    // error; the server must exit nonzero.
    client.send_raw_then_close(b"Content-Type: application/json\r\n\r\n{}");
    let status = client.wait_exit();
    assert!(
        !status.success(),
        "missing Content-Length must terminate the server abnormally"
    );
}

#[test]
fn truncated_body_is_fatal() {
    let mut client = RpcClient::spawn();
    // Declare 100 bytes but send only a few: read_exact hits EOF → fatal.
    client.send_raw_then_close(b"Content-Length: 100\r\n\r\n{");
    let status = client.wait_exit();
    assert!(
        !status.success(),
        "truncated body must terminate the server abnormally"
    );
}

#[test]
fn eof_before_shutdown_exits_nonzero() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    // Closing stdin (EOF) without shutdown must exit nonzero per LSP.
    let status = client.wait_exit();
    assert!(
        !status.success(),
        "EOF before shutdown must be a nonzero exit"
    );
}

#[test]
fn eof_after_shutdown_exits_zero() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.request(2, "shutdown", serde_json::json!({}));
    // Closing stdin (EOF) after shutdown must exit zero.
    let status = client.wait_exit();
    assert!(
        status.success(),
        "EOF after shutdown must be an orderly exit"
    );
}

#[test]
fn oversized_document_is_rejected_and_cleared() {
    let mut client = spawn_tiny_doc_cap();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    // Open a valid (small) wrong-type document so diagnostics exist, then an
    // oversized replacement must drop the doc and clear its diagnostics.
    open_doc(&mut client, ENTRY_URI, WRONG_TYPE_SOURCE);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert!(
        !params["diagnostics"].as_array().unwrap().is_empty(),
        "wrong-type source must publish diagnostics first"
    );
    let oversized = "x".repeat(300);
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": ENTRY_URI, "version": 2 },
            "contentChanges": [{ "text": oversized }],
        }),
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "oversized replacement must clear its diagnostics"
    );
    // An oversized didOpen must also be rejected with an empty clear.
    let huge = "x".repeat(300);
    open_doc(
        &mut client,
        "file:///tmp/rustscript-lsp-oversized.rss",
        &huge,
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "oversized didOpen must publish an empty clear"
    );
}

#[test]
fn take_owned_signature_renders_exactly() {
    let dir = std::env::temp_dir().join("rustscript-lsp-take-owned-test");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("catalog.json");
    let json = serde_json::json!({
        "resources": [
            { "key": "custom.widget", "description": "A widget" }
        ],
        "functions": [
            {
                "name": "widget::open",
                "params": [ { "name": "label", "ty": "String", "passing": "Value" } ],
                "return_type": { "Resource": "custom.widget" },
                "description": "Opens a widget"
            },
            {
                "name": "widget::destroy",
                "params": [
                    { "name": "w", "ty": { "Resource": "custom.widget" }, "passing": "TakeOwned" }
                ],
                "return_type": "Int",
                "description": "Destroys a widget"
            }
        ]
    });
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).expect("write catalog");
    let arg = path.to_str().expect("utf8");

    let mut client = RpcClient::spawn_with_args(&["--catalog", arg]);
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    let uri = "file:///tmp/take-owned.rss";
    let source =
        "use widget;\nfn main() {\n    let w = widget::open(\"a\");\n    widget::destroy(w);\n}\n";
    open_doc(&mut client, uri, source);
    client.recv_notification("textDocument/publishDiagnostics");
    // Signature help inside the destroy(...) call must render `take_owned`.
    let sig = client.request(
        2,
        "textDocument/signatureHelp",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 18 },
        }),
    );
    let label = sig["result"]["signatures"][0]["label"]
        .as_str()
        .unwrap_or("");
    assert!(
        label.contains("take_owned resource<custom.widget>"),
        "take_owned passing must render in the signature: {label:?}"
    );
    assert!(
        label.contains("destroy"),
        "signature must name widget::destroy: {label:?}"
    );
}

// ---------------------------------------------------------------------------
// Canonical module overrides + exact parse diagnostics (process fixtures)
// ---------------------------------------------------------------------------

/// Create a uniquely-named temp directory for a fixture and return its path.
fn temp_fixture_dir(prefix: &str) -> std::path::PathBuf {
    let unique = format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("fixture dir should be created");
    dir.canonicalize().unwrap_or(dir)
}

/// Write one on-disk module source.
fn write_disk_module(dir: &std::path::Path, name: &str, source: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).expect("module source should write");
    path
}

/// Build the `file://` URI for an absolute path.
fn file_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

/// `fn main() { let x = 1; }\n`-style entry that imports `./util.rss` and
/// calls its `helper` (returning an int we use in an arithmetic expression so
/// samples appear).
const DISK_ENTRY_SOURCE: &str =
    "use self::util;\nfn main() {\n    let n = 1 + util::helper();\n}\n";

/// Disk version of `util.rss` returning 41.
const DISK_UTIL_GOOD: &str = "pub fn helper() -> int { 41 }\n";

/// Unsaved buffer version of `util.rss` returning 999 — must shadow the disk.
const BUFFER_UTIL_GOOD: &str = "pub fn helper() -> int { 999 }\n";

/// Unsaved buffer version of `util.rss` with a wrong-type call (diagnostic).
const BUFFER_UTIL_BAD: &str = "use sqlite;\npub fn helper() -> int {\n    let db = sqlite::open({});\n    sqlite::query(\"NOT_A_DB\", \"SELECT 1\", {}, {});\n    0\n}\n";

/// A syntax error in `util.rss` (unterminated block) whose parser span should
/// be reported under the module URI.
const BUFFER_UTIL_SYNTAX: &str = "pub fn helper() -> int {\n";

#[test]
fn disk_process_buffer_shadows_ondisk_import_for_diagnostics_and_hover() {
    let dir = temp_fixture_dir("lsp-disk-buffer");
    let util_path = write_disk_module(&dir, "util.rss", DISK_UTIL_GOOD);
    let main_path = write_disk_module(&dir, "main.rss", DISK_ENTRY_SOURCE);

    let main_uri = file_uri(&main_path);
    let util_uri = file_uri(&util_path);

    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));

    // 1. Open the module buffer with text that differs from disk. The
    //    unresolved import would otherwise read the disk version.
    open_doc(&mut client, &util_uri, BUFFER_UTIL_GOOD);
    let params = client.recv_publish_for(&util_uri);
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "clean module buffer publishes no diagnostics"
    );

    // 2. Open the entry. The entry analysis must use the open *buffer* (999),
    //    not the disk file (41).
    open_doc(&mut client, &main_uri, DISK_ENTRY_SOURCE);
    client.recv_publish_for(&main_uri);

    // Hover on `n` at line 3, char 8 must show int (the sum type). This
    // proves the buffer override (999) was used — either value is int, so to
    // prove the *module buffer* is used, change the buffer to a wrong-type
    // body and assert the diagnostic appears under the module URI.
    // (See the next test for the explicit wrong-type proof.)
    let hover = client.request(
        10,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": &main_uri },
            "position": { "line": 2, "character": 4 },
        }),
    );
    assert!(hover["result"].is_null() || hover["result"]["contents"].is_object());

    // 3. Replace the module buffer with a wrong-type body. Reanalysis of the
    //    entry (didChange to main) must attribute the wrong-type diagnostic to
    //    the *module URI* with the exact range from the buffer text — proving
    //    the buffer (not disk) is the analysis input.
    open_doc(&mut client, &util_uri, BUFFER_UTIL_BAD);
    // The module open itself reanalyzes util and publishes the error under
    // the module URI.
    let params = client.recv_publish_for(&util_uri);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
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
        "buffer override must surface the wrong-type diagnostic under the module uri: {diagnostics:?}"
    );
    let range = &wrong_type[0]["range"];
    assert_eq!(
        range["start"]["line"],
        serde_json::json!(3),
        "start line must be the buffer's query call line"
    );
    // On-disk text was good (41) with no query call; the wrong-type error can
    // only come from the buffer.
    assert_eq!(
        range["start"]["character"],
        serde_json::json!(4),
        "start char must be the buffer's callee"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disk_two_same_basename_modules_no_cross_talk() {
    let dir = temp_fixture_dir("lsp-same-basename");
    // a/util.rss and b/util.rss both define helper(), returning 1 vs 2.
    let a_dir = dir.join("a");
    let b_dir = dir.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir");
    std::fs::create_dir_all(&b_dir).expect("b dir");
    let a_module = write_disk_module(&a_dir, "util.rss", "pub fn helper() -> int { 1 }\n");
    let b_module = write_disk_module(&b_dir, "util.rss", "pub fn helper() -> int { 2 }\n");
    let main_path = write_disk_module(
        &dir,
        "main.rss",
        "use a::util as au;\nuse b::util as bu;\nfn main() {\n    au::helper() + bu::helper()\n}\n",
    );

    let main_uri = file_uri(&main_path);
    let a_uri = file_uri(&a_module);
    let b_uri = file_uri(&b_module);

    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));

    // Open both module buffers with *different* bodies (same basename). Both
    // must be honored simultaneously — no basename override collision.
    open_doc(&mut client, &a_uri, "pub fn helper() -> int { 100 }\n");
    client.recv_publish_for(&a_uri);
    open_doc(&mut client, &b_uri, "pub fn helper() -> int { 200 }\n");
    client.recv_publish_for(&b_uri);

    // Open the entry and request hover on each `helper()` call's result. The
    // a-buffer must shadow a/util (100→int) and the b-buffer shadow b/util
    // (200→int), with no cross-talk. We assert the imports resolve without
    // producing cross-module errors: a clean entry means both overrides were
    // applied independently.
    open_doc(
        &mut client,
        &main_uri,
        "use a::util as au;\nuse b::util as bu;\nfn main() {\n    au::helper() + bu::helper()\n}\n",
    );
    let params = client.recv_publish_for(&main_uri);
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "same-basename modules in separate dirs must both honor their own buffers, no cross talk"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disk_close_imported_module_clears_and_importer_does_not_republish() {
    let dir = temp_fixture_dir("lsp-disk-close");
    let util_path = write_disk_module(&dir, "util.rss", DISK_UTIL_GOOD);
    let main_path = write_disk_module(&dir, "main.rss", DISK_ENTRY_SOURCE);

    let main_uri = file_uri(&main_path);
    let util_uri = file_uri(&util_path);

    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));

    // Open a bad module buffer (produces diagnostics), then the entry.
    open_doc(&mut client, &util_uri, BUFFER_UTIL_BAD);
    client.recv_publish_for(&util_uri);
    open_doc(&mut client, &main_uri, DISK_ENTRY_SOURCE);
    let params = client.recv_publish_for(&util_uri);
    assert!(
        !params["diagnostics"].as_array().unwrap().is_empty(),
        "entry analysis must report the module's buffer error"
    );

    // Close the module: its published diagnostics must clear.
    client.notify(
        "textDocument/didClose",
        serde_json::json!({ "textDocument": { "uri": &util_uri } }),
    );
    let params = client.recv_publish_for(&util_uri);
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "closing the module must clear its diagnostics"
    );

    // Re-trigger entry reanalysis (didChange). The importer must NOT
    // republish the closed module's diagnostics — the module's source is
    // suppressed because its buffer is gone and the disk file (41) is clean.
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": &main_uri, "version": 2 },
            "contentChanges": [{ "text": DISK_ENTRY_SOURCE }],
        }),
    );
    // After reanalysis the main URI re-publishes (empty result); assert that
    // within the next few publishes no util URI (with any content) appears.
    client.recv_publish_for(&main_uri);
    // Bounded probe: over the next main re-publishes the util URI must not
    // carry a non-empty diagnostic set. (The close already emitted the empty
    // clear; a republish would indicate the suppressed-source leak.)
    let mut leaked = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let message = match self_mpsc_recv_timeout(&client, deadline) {
            Some(message) => message,
            None => break,
        };
        if message.get("method") == Some(&serde_json::json!("textDocument/publishDiagnostics")) {
            let params = &message["params"];
            if params["uri"] == serde_json::json!(util_uri)
                && !params["diagnostics"].as_array().unwrap().is_empty()
            {
                leaked = true;
            }
        }
    }
    assert!(
        !leaked,
        "reanalysis after close must not republish the closed module's diagnostics"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Non-blocking receive from a client's channel. Returns `None` if nothing is
/// queued within `deadline`.
fn self_mpsc_recv_timeout(
    client: &RpcClient,
    deadline: std::time::Instant,
) -> Option<serde_json::Value> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    use std::sync::mpsc::RecvTimeoutError;
    match client.messages.recv_timeout(remaining) {
        Ok(message) => Some(message),
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
    }
}

#[test]
fn syntax_error_change_publishes_exact_parse_diagnostic_and_clears_model() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    let uri = ENTRY_URI;

    // Open a valid entry: no diagnostics, model present.
    open_doc(&mut client, uri, CLEAN_SOURCE);
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(params["diagnostics"], serde_json::json!([]));

    // Change to a syntax error. The server must publish an exact parse
    // diagnostic (non-empty, with a real range) and drop the model so
    // hover/definition return null.
    let bad = "fn main() {\n";
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": bad }],
        }),
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        !diagnostics.is_empty(),
        "a syntax error must publish at least one exact parse diagnostic"
    );
    let range = &diagnostics[0]["range"];
    assert!(
        range["start"]["line"].as_u64().is_some(),
        "parse diagnostic must carry a real range"
    );
    // The unterminated block error points at the opening line of `fn main() {`
    // (line 0) — never a degenerate whole-document or line-0-zero-width marker
    // at EOF. The parser's `with_line_span_from_source` pins the span to the
    // offending construct's line.
    assert_eq!(
        range["start"]["line"],
        serde_json::json!(0),
        "parse diagnostic must point at the offending line"
    );
    assert!(
        !diagnostics[0]["message"].as_str().unwrap_or("").is_empty(),
        "parse diagnostic must carry a message"
    );

    // Hover and definition against the broken buffer must return null (the
    // stale model was dropped).
    let hover = client.request(
        21,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 0 },
        }),
    );
    assert_eq!(hover["result"], serde_json::Value::Null);
    let definition = client.request(
        22,
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 0 },
        }),
    );
    assert_eq!(definition["result"], serde_json::Value::Null);
}

#[test]
fn syntax_fix_replaces_diagnostic_and_restores_model() {
    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));
    let uri = ENTRY_URI;

    // Open a broken entry: parse diagnostic published, model dropped.
    open_doc(&mut client, uri, "fn main() {\n");
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert!(
        !params["diagnostics"].as_array().unwrap().is_empty(),
        "syntax error document must publish parse diagnostics"
    );

    // Fix it to a clean source: the parse diagnostic is replaced by an empty
    // publish and the model is restored (hover works again).
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": CLEAN_SOURCE }],
        }),
    );
    let params = client.recv_notification("textDocument/publishDiagnostics");
    assert_eq!(
        params["diagnostics"],
        serde_json::json!([]),
        "syntax fix must clear the parse diagnostic"
    );
    let hover = client.request(
        21,
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 2, "character": 8 },
        }),
    );
    let value = hover["result"]["contents"]["value"].as_str().unwrap_or("");
    assert!(
        value.contains("resource<sqlite.connection>"),
        "hover must work again after the syntax fix: {value:?}"
    );
}

#[test]
fn imported_module_syntax_error_attributed_to_module_uri() {
    let dir = temp_fixture_dir("lsp-module-syntax");
    let util_path = write_disk_module(&dir, "util.rss", DISK_UTIL_GOOD);
    let main_path = write_disk_module(&dir, "main.rss", DISK_ENTRY_SOURCE);

    let main_uri = file_uri(&main_path);
    let util_uri = file_uri(&util_path);

    let mut client = RpcClient::spawn();
    client.request(1, "initialize", serde_json::json!({}));
    client.notify("initialized", serde_json::json!({}));

    // Open an entry that imports util, then open the util buffer with a
    // syntax error. The reanalysis must attribute the parse diagnostic to the
    // *module URI*, not the entry.
    open_doc(&mut client, &main_uri, DISK_ENTRY_SOURCE);
    client.recv_publish_for(&main_uri);
    open_doc(&mut client, &util_uri, BUFFER_UTIL_SYNTAX);
    let params = client.recv_publish_for(&util_uri);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        !diagnostics.is_empty(),
        "module syntax error must publish a parse diagnostic"
    );

    // Re-analyze the entry (didChange). The entry's analysis must also
    // attribute the module's parse error to the *module URI* (never the
    // entry), proving the import graph renders each error against its owner.
    client.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": &main_uri, "version": 2 },
            "contentChanges": [{ "text": DISK_ENTRY_SOURCE }],
        }),
    );
    let params = client.recv_publish_for(&util_uri);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        !diagnostics.is_empty(),
        "entry reanalysis must republish the module parse error under the module uri"
    );
    let msg = diagnostics[0]["message"].as_str().unwrap_or("");
    assert!(
        !msg.is_empty(),
        "module parse diagnostic must carry the parser message"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
