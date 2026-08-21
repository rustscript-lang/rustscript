//! Resource-aware RustScript language server (LSP over stdio).
//!
//! A self-contained stdio LSP adapter backed exclusively by the compiler's
//! [`SemanticModel`] query surface. It implements the JSON-RPC/LSP lifecycle
//! (initialize / initialized / shutdown / exit), full-sync text document
//! synchronization (didOpen / didChange / didClose), and pushes semantic
//! diagnostics after every analysis. Language features:
//!
//! * `textDocument/hover` — the inferred schema at the cursor, rendered with
//!   exact opaque resource keys (`resource<sqlite.connection>`).
//! * `textDocument/signatureHelp` — the exact resolved host call signature
//!   including passing modes (`borrow` / `borrow_mut` / `take_owned`).
//! * `textDocument/completion` — visible locals/functions plus catalog host
//!   functions with resource-aware detail.
//! * `textDocument/definition` — local/function definitions in real sources,
//!   and deterministic virtual locations for catalog host definitions
//!   (`host://<name>/<arity>`) backed by the `rustscript-host://` document
//!   content endpoint.
//!
//! The server loads the same standard `HostApiCatalog` snapshot the compiler
//! uses (composed from the sqlite/io/http extension catalogs of this build).
//! A custom catalog may be supplied with `--catalog <file.json>`; the catalog
//! is validated by the same serde path the compiler uses, and a fingerprint /
//! schema mismatch is reported as an explicit startup error — resource types
//! are never coerced to `int` and a mismatched catalog is never silently
//! used.
//!
//! Robustness: messages are size-bounded (see [`MAX_MESSAGE_BYTES`]),
//! malformed requests produce JSON-RPC errors (never panics), invalid UTF-16
//! positions and unknown URIs return `None`/empty results, and EOF/`exit`
//! terminates orderly.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustscript::{
    CompileSourceFileOptions, HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing,
    HostTypeSchema, SemanticDiagnostic, SemanticModel, SourcePosition,
    analyze_source_from_string_with_options,
};

/// Hard cap on a single JSON-RPC message payload (LSP bodies are small; a
/// pathological client cannot exhaust memory).
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Hard cap on an individual document's text (editors can send huge buffers;
/// bound reanalysis cost).
const MAX_DOCUMENT_CHARS: usize = 8 * 1024 * 1024;
/// Scheme used for virtual host-definition documents.
const HOST_SCHEME: &str = "rustscript-host";

/// A JSON-RPC message with an optional id (notifications omit it).
#[derive(Debug, Clone)]
struct RpcMessage {
    id: Option<serde_json::Value>,
    method: String,
    params: serde_json::Value,
}

/// The outcome of reading one message: a parsed message, or a recoverable
/// parse error to respond with (`-32700`), or a fatal framing error.
enum ReadOutcome {
    /// A parsed message.
    Message(RpcMessage),
    /// EOF before any header: orderly shutdown of the stream.
    Eof,
    /// A recoverable malformed-payload error; respond and keep reading.
    ParseError(String),
    /// A fatal framing error (bad headers, over-limit, truncated frame).
    Fatal(String),
}

/// Parse a `Content-Length` framed JSON-RPC message from a reader.
///
/// Returns [`ReadOutcome::Message`] on success, [`ReadOutcome::Eof`] on
/// clean EOF before any header, [`ReadOutcome::ParseError`] for malformed
/// JSON bodies (recoverable), and [`ReadOutcome::Fatal`] for broken framing
/// or over-limit payloads (the stream cannot be resynced).
fn read_message(reader: &mut impl BufRead) -> ReadOutcome {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(err) => return ReadOutcome::Fatal(format!("failed reading header: {err}")),
        };
        if n == 0 {
            // EOF. If we have already seen headers this is a truncated frame.
            if content_length.is_some() {
                return ReadOutcome::Fatal("unexpected EOF inside message headers".to_string());
            }
            return ReadOutcome::Eof;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return ReadOutcome::Fatal(format!("malformed header line: {line:?}"));
        };
        if name.eq_ignore_ascii_case("content-length") {
            let parsed: usize = match value.trim().parse() {
                Ok(parsed) => parsed,
                Err(_) => return ReadOutcome::Fatal(format!("invalid Content-Length: {value:?}")),
            };
            if parsed > MAX_MESSAGE_BYTES {
                return ReadOutcome::Fatal(format!("message too large: {parsed} bytes"));
            }
            content_length = Some(parsed);
        }
        // Content-Type is ignored (we always speak JSON).
    }
    let Some(content_length) = content_length else {
        return ReadOutcome::Fatal("missing Content-Length header".to_string());
    };
    let mut body = vec![0u8; content_length];
    if let Err(err) = reader.read_exact(&mut body) {
        return ReadOutcome::Fatal(format!("failed reading body: {err}"));
    }
    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => return ReadOutcome::ParseError("message body is not valid UTF-8".to_string()),
    };
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => return ReadOutcome::ParseError(format!("invalid JSON-RPC payload: {err}")),
    };
    let Some(method) = value.get("method").and_then(serde_json::Value::as_str) else {
        return ReadOutcome::ParseError("message has no string method".to_string());
    };
    let id = value.get("id").cloned();
    let params = value
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    ReadOutcome::Message(RpcMessage {
        id,
        method: method.to_string(),
        params,
    })
}

/// Write a JSON-RPC message with `Content-Length` framing.
fn write_message(out: &mut impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).expect("LSP response must serialize");
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

/// Build a JSON-RPC success result.
fn result_message(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC error response.
fn error_message(id: &serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Standard JSON-RPC error code used for unknown methods.
const RPC_METHOD_NOT_FOUND: i64 = -32601;

/// The standard host API catalog for this build: sqlite + io + http
/// extension catalogs composed into one validated snapshot, exactly like the
/// compiler's host surface.
fn standard_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    for catalog in [
        rustscript::sqlite_host_catalog(),
        rustscript::io_host_catalog(),
        rustscript::http_host_catalog(),
    ] {
        for resource in catalog.resources() {
            builder.resource(resource.clone());
        }
        for function in catalog.functions() {
            builder.function(function.clone());
        }
    }
    Arc::new(builder.build().expect("standard catalog must be valid"))
}

/// Load a custom catalog from a JSON file (the `HostApiCatalog` serde shape).
/// The serde path re-validates everything exactly like the builder, so a
/// fingerprint/schema mismatch cannot be silently accepted.
fn load_catalog_file(path: &Path) -> Result<Arc<HostApiCatalog>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("failed reading catalog file {}: {err}", path.display()))?;
    let catalog: HostApiCatalog = serde_json::from_str(&text)
        .map_err(|err| format!("invalid host API catalog {}: {err}", path.display()))?;
    Ok(Arc::new(catalog))
}

// ---------------------------------------------------------------------------
// Position conversion (LSP <-> SourcePosition)
// ---------------------------------------------------------------------------

/// Convert an LSP `Position` (0-indexed line, UTF-16 code-unit column) to a
/// byte offset within `text`. Returns `None` for out-of-range positions.
fn lsp_position_to_offset(text: &str, line: u32, character: u32) -> Option<usize> {
    let mut current_line = 0u32;
    let mut offset = 0usize;
    for (idx, chunk) in text.split_inclusive('\n').enumerate() {
        let _ = idx;
        if current_line == line {
            // Walk the line's chars accumulating UTF-16 code units.
            let line_text = chunk.trim_end_matches(['\n', '\r']);
            let mut utf16_seen = 0u32;
            for (byte_idx, ch) in line_text.char_indices() {
                if utf16_seen >= character {
                    return Some(offset + byte_idx);
                }
                utf16_seen += ch.len_utf16() as u32;
            }
            // Cursor at or past the end of the line.
            return Some(offset + line_text.len());
        }
        offset += chunk.len();
        current_line += 1;
    }
    // Line beyond the text: clamp to EOF only when the requested line is the
    // (empty) line after a trailing newline, else reject.
    if line == current_line {
        Some(offset)
    } else {
        None
    }
}

/// Convert a byte offset to an LSP `Position` (0-indexed line + UTF-16
/// column). Returns `None` if the offset is not on a char boundary.
fn offset_to_lsp_position(text: &str, offset: usize) -> Option<(u32, u32)> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, chunk) in text.split_inclusive('\n').enumerate() {
        let _ = idx;
        if offset <= line_start + chunk.len() {
            let line_text = &text[line_start..offset.min(line_start + chunk.len())];
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let utf16: u32 = line_text.chars().map(|c| c.len_utf16() as u32).sum();
            return Some((line, utf16));
        }
        line_start += chunk.len();
        line += 1;
    }
    // Offset at EOF (after final newline).
    let line_text = &text[line_start..];
    let utf16: u32 = line_text.chars().map(|c| c.len_utf16() as u32).sum();
    Some((line, utf16))
}

// ---------------------------------------------------------------------------
// Document store
// ---------------------------------------------------------------------------

/// One open document: its URI, filesystem path (derived from the URI), the
/// current buffer text, and the last analysis result (if any).
struct Document {
    uri: String,
    path: PathBuf,
    text: String,
    model: Option<SemanticModel>,
}

impl Document {
    fn new(uri: String, path: PathBuf, text: String) -> Self {
        Self {
            uri,
            path,
            text,
            model: None,
        }
    }
}

/// Convert an LSP document URI to a filesystem path. Supports `file://` URIs
/// (percent-decoded); other schemes map to a synthetic in-memory path rooted
/// under the host scheme so virtual host documents stay addressable.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path_str = percent_decode(rest);
    // Strip the leading slash from the authority-less form.
    let path_str = path_str.strip_prefix('/').unwrap_or(&path_str);
    Some(PathBuf::from(path_str))
}

/// Minimal percent-decoding for URI paths (LSP file URIs percent-encode
/// spaces and non-ASCII).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Render a host parameter with its passing mode, e.g.
/// `connection: borrow resource<sqlite.connection>`.
fn render_param(name: &str, ty: &HostTypeSchema, passing: HostParamPassing) -> String {
    let mode = match passing {
        HostParamPassing::Value => "",
        HostParamPassing::Borrow => "borrow ",
        HostParamPassing::BorrowMut => "borrow_mut ",
        HostParamPassing::TakeOwned => "take_owned ",
    };
    format!("{name}: {mode}{ty}")
}

/// Render a full host signature: `sqlite::query(connection: borrow
/// resource<sqlite.connection>, sql: string) -> map<unknown>`.
fn render_host_signature(schema: &HostFunctionSchema) -> String {
    let params: Vec<String> = schema
        .params
        .iter()
        .map(|p| render_param(&p.name, &p.ty, p.passing))
        .collect();
    format!(
        "{}({}) -> {}",
        schema.name,
        params.join(", "),
        schema.return_type
    )
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct LspServer {
    catalog: Arc<HostApiCatalog>,
    /// uri -> open document (buffer overrides disk).
    documents: HashMap<String, Document>,
    /// Map from a source file name (as recorded in the SourceMap) to the URI
    /// that should be reported for diagnostics/locations in that source.
    source_name_to_uri: HashMap<String, String>,
    shutdown_requested: bool,
}

impl LspServer {
    fn new(catalog: Arc<HostApiCatalog>) -> Self {
        Self {
            catalog,
            documents: HashMap::new(),
            source_name_to_uri: HashMap::new(),
            shutdown_requested: false,
        }
    }

    /// The compile options for this server: the exact catalog snapshot plus
    /// module-source overrides for every open document (so an open buffer
    /// shadows the on-disk module it corresponds to).
    fn compile_options(&self) -> CompileSourceFileOptions {
        let mut options =
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&self.catalog));
        for doc in self.documents.values() {
            let spec = doc.path.to_string_lossy().replace('\\', "/");
            options = options.with_module_override_source(spec, doc.text.clone());
            // Also register the import-spec form (./foo.rss) for relative
            // imports of open documents.
            if let Some(file_name) = doc.path.file_name().and_then(|n| n.to_str()) {
                options =
                    options.with_module_override_source(file_name.to_string(), doc.text.clone());
            }
        }
        options
    }

    /// Analyze (or reanalyze) the document at `uri` with its current buffer
    /// text. Stale diagnostics for other documents are cleared by the caller.
    fn analyze_document(&mut self, uri: &str) {
        let options = self.compile_options();
        let Some(doc) = self.documents.get_mut(uri) else {
            return;
        };
        let path = doc.path.clone();
        let text = doc.text.clone();
        let result = analyze_source_from_string_with_options(&path, &text, options);
        let model = match result {
            Ok(model) => Some(model),
            Err(_) => {
                // A source-path error (unreadable import etc.) still needs a
                // model for diagnostics. `SemanticModel` is not `Clone`, so
                // preserve the previous model (if any) by moving it out;
                // otherwise the entry document keeps no model and the failure
                // surfaces through the diagnostics channel.
                doc.model.take()
            }
        };
        doc.model = model;
        // Rebuild source-name -> uri mapping from the model's SourceMap.
        // Collect the source names first (the doc borrow must end before the
        // immutable `self` borrow in `uri_for_source_name`).
        let source_names: Vec<String> = match &doc.model {
            Some(model) => {
                let mut names = Vec::new();
                for source_id in 0.. {
                    let Some(name) = model.sources().file_name(source_id) else {
                        break;
                    };
                    names.push(name.to_string());
                }
                names
            }
            None => Vec::new(),
        };
        self.source_name_to_uri.clear();
        for name in source_names {
            let mapped = self.uri_for_source_name(&name);
            self.source_name_to_uri.insert(name, mapped);
        }
    }

    /// Map a SourceMap file name back to a client URI. Files that are open
    /// documents map to their document URI; other real files map to a
    /// `file://` URI; host/virtual names map to the host scheme.
    fn uri_for_source_name(&self, name: &str) -> String {
        if let Some(doc) = self
            .documents
            .values()
            .find(|doc| doc.path.to_string_lossy() == name)
        {
            return doc.uri.clone();
        }
        // Normalize: the compiler records the path as passed; compare with
        // each document's path string form.
        let normalized = name.replace('\\', "/");
        for doc in self.documents.values() {
            let doc_path = doc.path.to_string_lossy().replace('\\', "/");
            if doc_path == normalized {
                return doc.uri.clone();
            }
        }
        if name.starts_with("host://") || name.starts_with(HOST_SCHEME) {
            return format!("{HOST_SCHEME}://{}", name.trim_start_matches("host://"));
        }
        format!("file://{}", name)
    }

    /// All diagnostics for the document at `uri` (from its model), as an LSP
    /// `Diagnostic[]`.
    fn lsp_diagnostics(&self, uri: &str) -> Vec<serde_json::Value> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(model) = &doc.model else {
            return Vec::new();
        };
        let text = &doc.text;
        let mut out = Vec::new();
        for diag in model.diagnostics() {
            if let Some(value) = self.lsp_diagnostic(model, text, &diag) {
                out.push(value);
            }
        }
        out
    }

    fn lsp_diagnostic(
        &self,
        model: &SemanticModel,
        text: &str,
        diag: &SemanticDiagnostic,
    ) -> Option<serde_json::Value> {
        let span = diag.span?;
        let (lo, hi) = (span.lo.min(text.len()), span.hi.min(text.len()));
        let start = offset_to_lsp_position(text, lo)?;
        let end = offset_to_lsp_position(text, hi)?;
        let mut value = serde_json::json!({
            "range": { "start": { "line": start.0, "character": start.1 },
                       "end": { "line": end.0, "character": end.1 } },
            "severity": 1,
            "source": "rustscript",
            "message": diag.message,
        });
        if let Some(code) = &diag.code {
            value["code"] = serde_json::Value::String(code.clone());
        }
        let _ = model;
        Some(value)
    }

    /// Publish diagnostics for every open document (with an empty array for
    /// documents whose analysis produced none).
    fn publish_all_diagnostics(&self, out: &mut impl Write) -> std::io::Result<()> {
        let uris: Vec<String> = self.documents.keys().cloned().collect();
        for uri in uris {
            let diagnostics = self.lsp_diagnostics(&uri);
            let params = serde_json::json!({
                "uri": uri,
                "diagnostics": diagnostics,
            });
            write_message(
                out,
                &serde_json::json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params }),
            )?;
        }
        Ok(())
    }

    /// Clear diagnostics for a single document (on close).
    fn clear_diagnostics(&self, out: &mut impl Write, uri: &str) -> std::io::Result<()> {
        let params = serde_json::json!({
            "uri": uri,
            "diagnostics": [],
        });
        write_message(
            out,
            &serde_json::json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params }),
        )
    }

    /// Resolve an LSP text-document position against an open document.
    fn source_position(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<(SourcePosition, &SemanticModel)> {
        let doc = self.documents.get(uri)?;
        let model = doc.model.as_ref()?;
        let offset = lsp_position_to_offset(&doc.text, line, character)?;
        // Find the SourceId for this document inside the model's SourceMap.
        let source_id = model
            .sources()
            .source_id_by_name(&doc.path.to_string_lossy())
            .or_else(|| {
                // Fall back to the first source whose name ends with this
                // document's file name.
                let file_name = doc.path.file_name()?.to_str()?;
                let mut found = None;
                for id in 0.. {
                    let Some(name) = model.sources().file_name(id) else {
                        break;
                    };
                    if name.ends_with(file_name) {
                        found = Some(id);
                        break;
                    }
                }
                found
            })?;
        Some((SourcePosition::new(source_id, offset), model))
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

impl LspServer {
    /// Handle a single request/notification. Returns the response to send,
    /// or `None` for notifications.
    fn handle(
        &mut self,
        msg: &RpcMessage,
        out: &mut impl Write,
    ) -> std::io::Result<Option<serde_json::Value>> {
        let method = msg.method.as_str();
        match method {
            // ---- lifecycle ----
            "initialize" => {
                let result = self.initialize_response();
                Ok(Some(result_message(
                    msg.id.as_ref().unwrap_or(&serde_json::Value::Null),
                    result,
                )))
            }
            "initialized" => Ok(None),
            "shutdown" => {
                self.shutdown_requested = true;
                Ok(Some(result_message(
                    msg.id.as_ref().unwrap_or(&serde_json::Value::Null),
                    serde_json::Value::Null,
                )))
            }
            "exit" => {
                // exit is a notification; the loop terminates on it.
                Ok(None)
            }
            "$/cancelRequest" => Ok(None),
            "$/setTrace" => Ok(None),
            // ---- text document sync ----
            "textDocument/didOpen" => {
                self.handle_did_open(&msg.params, out)?;
                Ok(None)
            }
            "textDocument/didChange" => {
                self.handle_did_change(&msg.params, out)?;
                Ok(None)
            }
            "textDocument/didClose" => {
                self.handle_did_close(&msg.params, out)?;
                Ok(None)
            }
            // ---- language features ----
            "textDocument/hover" => {
                let id = msg.id.as_ref().unwrap_or(&serde_json::Value::Null);
                Ok(Some(result_message(id, self.handle_hover(&msg.params))))
            }
            "textDocument/signatureHelp" => {
                let id = msg.id.as_ref().unwrap_or(&serde_json::Value::Null);
                Ok(Some(result_message(
                    id,
                    self.handle_signature_help(&msg.params),
                )))
            }
            "textDocument/completion" => {
                let id = msg.id.as_ref().unwrap_or(&serde_json::Value::Null);
                Ok(Some(result_message(
                    id,
                    self.handle_completion(&msg.params),
                )))
            }
            "textDocument/definition" => {
                let id = msg.id.as_ref().unwrap_or(&serde_json::Value::Null);
                Ok(Some(result_message(
                    id,
                    self.handle_definition(&msg.params),
                )))
            }
            // ---- custom document content endpoint ----
            "rustscript-host/documentContent" => {
                let id = msg.id.as_ref().unwrap_or(&serde_json::Value::Null);
                Ok(Some(result_message(
                    id,
                    self.handle_host_document_content(&msg.params),
                )))
            }
            "workspace/symbol" | "textDocument/documentSymbol" | "textDocument/references" => {
                // Not implemented: return empty results per LSP (null result).
                let id = msg.id.as_ref().unwrap_or(&serde_json::Value::Null);
                Ok(Some(result_message(id, serde_json::Value::Null)))
            }
            _ => {
                if msg.id.is_some() {
                    Ok(Some(error_message(
                        msg.id.as_ref().unwrap(),
                        RPC_METHOD_NOT_FOUND,
                        &format!("method not found: {method}"),
                    )))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn initialize_response(&self) -> serde_json::Value {
        serde_json::json!({
            "capabilities": {
                "textDocumentSync": { "openClose": true, "change": 1 },
                "hoverProvider": true,
                "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
                "completionProvider": { "triggerCharacters": [".", ":"] },
                "definitionProvider": true,
            },
            "serverInfo": {
                "name": "rustscript-lsp",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })
    }

    fn handle_did_open(
        &mut self,
        params: &serde_json::Value,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        let Some(uri) = params["textDocument"]["uri"].as_str() else {
            return Ok(());
        };
        let Some(text) = params["textDocument"]["text"].as_str() else {
            return Ok(());
        };
        let uri = uri.to_string();
        if text.chars().count() > MAX_DOCUMENT_CHARS {
            // Oversized buffer: drop the document (do not analyze).
            self.documents.remove(&uri);
            return Ok(());
        }
        let path =
            uri_to_path(&uri).unwrap_or_else(|| PathBuf::from(uri.trim_start_matches("file://")));
        self.documents.insert(
            uri.clone(),
            Document::new(uri.clone(), path, text.to_string()),
        );
        self.analyze_document(&uri);
        self.publish_all_diagnostics(out)
    }

    fn handle_did_change(
        &mut self,
        params: &serde_json::Value,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        let Some(uri) = params["textDocument"]["uri"].as_str() else {
            return Ok(());
        };
        let uri = uri.to_string();
        // Full-sync: the last change's text is the whole buffer.
        let changes = params["contentChanges"].as_array();
        let Some(changes) = changes else {
            return Ok(());
        };
        let Some(last) = changes.last() else {
            return Ok(());
        };
        let Some(text) = last["text"].as_str() else {
            return Ok(());
        };
        if text.chars().count() > MAX_DOCUMENT_CHARS {
            self.documents.remove(&uri);
            return self.clear_diagnostics(out, &uri);
        }
        let Some(doc) = self.documents.get_mut(&uri) else {
            return Ok(());
        };
        doc.text = text.to_string();
        self.analyze_document(&uri);
        self.publish_all_diagnostics(out)
    }

    fn handle_did_close(
        &mut self,
        params: &serde_json::Value,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        let Some(uri) = params["textDocument"]["uri"].as_str() else {
            return Ok(());
        };
        let uri = uri.to_string();
        self.documents.remove(&uri);
        self.source_name_to_uri.remove(&uri);
        self.clear_diagnostics(out, &uri)
    }

    fn handle_hover(&self, params: &serde_json::Value) -> serde_json::Value {
        let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
        let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
        let character = params["position"]["character"].as_u64().unwrap_or(0) as u32;
        let Some((position, model)) = self.source_position(uri, line, character) else {
            return serde_json::Value::Null;
        };
        match model.inferred_schema_at(position) {
            Some(schema) => {
                let contents = serde_json::json!({
                    "kind": "markdown",
                    "value": format!("```rustscript\n{schema}\n```"),
                });
                serde_json::json!({ "contents": contents })
            }
            None => serde_json::Value::Null,
        }
    }

    fn handle_signature_help(&self, params: &serde_json::Value) -> serde_json::Value {
        let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
        let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
        let character = params["position"]["character"].as_u64().unwrap_or(0) as u32;
        let Some((position, model)) = self.source_position(uri, line, character) else {
            return serde_json::Value::Null;
        };
        match model.callable_signature_at(position) {
            Some(schema) => {
                let label = render_host_signature(&schema);
                let params_list: Vec<String> = schema
                    .params
                    .iter()
                    .map(|p| render_param(&p.name, &p.ty, p.passing))
                    .collect();
                // The active parameter is the one containing the cursor.
                // SemanticModel does not expose the active index; LSP allows
                // omitting it, so clients render the whole signature.
                let signature = serde_json::json!({
                    "label": label,
                    "documentation": { "kind": "markdown", "value": schema.description },
                    "parameters": params_list.iter().map(|p| serde_json::json!({ "label": p })).collect::<Vec<_>>(),
                });
                serde_json::json!({ "signatures": [signature] })
            }
            None => serde_json::Value::Null,
        }
    }

    fn handle_completion(&self, params: &serde_json::Value) -> serde_json::Value {
        let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
        let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
        let character = params["position"]["character"].as_u64().unwrap_or(0) as u32;
        let Some((position, model)) = self.source_position(uri, line, character) else {
            return serde_json::Value::Null;
        };
        let completions = model.completions_at(position);
        let items: Vec<serde_json::Value> = completions
            .iter()
            .map(|c| {
                let kind = match c.kind {
                    rustscript::CompletionItemKind::Variable => 6,
                    rustscript::CompletionItemKind::Function => 3,
                    rustscript::CompletionItemKind::Resource => 7,
                    rustscript::CompletionItemKind::Keyword => 14,
                };
                let mut item = serde_json::json!({
                    "label": c.label,
                    "kind": kind,
                });
                if let Some(detail) = &c.detail {
                    item["detail"] = serde_json::Value::String(detail.clone());
                }
                if let Some(docs) = &c.docs {
                    item["documentation"] = serde_json::Value::String(docs.clone());
                }
                item
            })
            .collect();
        serde_json::json!({ "isIncomplete": false, "items": items })
    }

    fn handle_definition(&self, params: &serde_json::Value) -> serde_json::Value {
        let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
        let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
        let character = params["position"]["character"].as_u64().unwrap_or(0) as u32;
        let Some((position, model)) = self.source_position(uri, line, character) else {
            return serde_json::Value::Null;
        };
        match model.definition_at(position) {
            Some(def) => {
                // The definition span may live in another source (module
                // symbol). Resolve the target URI from the SourceMap.
                let target_uri = self.uri_for_span(model, def.span);
                if def.label.starts_with("host://") || def.label.starts_with(HOST_SCHEME) {
                    // Virtual host definition: deterministic location in the
                    // virtual host document. The host URI encodes the name
                    // and arity so the location is stable.
                    let (name, arity) = parse_host_label(&def.label);
                    let host_uri = format!("{HOST_SCHEME}://{name}/{arity}");
                    // The virtual document has a single line; the definition
                    // spans the whole function entry.
                    return serde_json::json!([{
                        "uri": host_uri,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 },
                        }
                    }]);
                }
                // Real source location.
                let Some(text) = model
                    .sources()
                    .file(def.span.source_id)
                    .map(|f| f.text.as_str())
                else {
                    return serde_json::Value::Null;
                };
                let lo = def.span.lo.min(text.len());
                let hi = def.span.hi.min(text.len());
                let Some(start) = offset_to_lsp_position(text, lo) else {
                    return serde_json::Value::Null;
                };
                let Some(end) = offset_to_lsp_position(text, hi) else {
                    return serde_json::Value::Null;
                };
                serde_json::json!([{
                    "uri": target_uri,
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 },
                    }
                }])
            }
            None => serde_json::Value::Null,
        }
    }

    /// Map a definition span's source to a client URI.
    fn uri_for_span(&self, model: &SemanticModel, span: rustscript::Span) -> String {
        let name = model
            .sources()
            .file_name(span.source_id)
            .unwrap_or("unknown");
        self.uri_for_source_name(name)
    }

    /// Serve the content of a virtual host document: the rendered signature
    /// and description for the catalog function named by the URI.
    fn handle_host_document_content(&self, params: &serde_json::Value) -> serde_json::Value {
        let uri = params["uri"].as_str().unwrap_or("");
        let Some(rest) = uri.strip_prefix(&format!("{HOST_SCHEME}://")) else {
            return serde_json::Value::Null;
        };
        let (name, arity) = split_host_uri(rest);
        let matches: Vec<&HostFunctionSchema> = self
            .catalog
            .functions()
            .iter()
            .filter(|f| f.name == name)
            .filter(|f| f.params.len() == arity)
            .collect();
        let content = if matches.is_empty() {
            format!("// Unknown host function: {name} (arity {arity})")
        } else {
            let mut lines = Vec::new();
            for schema in &matches {
                lines.push(render_host_signature(schema));
                if !schema.description.is_empty() {
                    lines.push(format!("// {}", schema.description));
                }
            }
            lines.join("\n")
        };
        serde_json::json!({ "uri": uri, "content": content })
    }
}

/// Parse a host definition label (`host://<name>/<arity> — <desc>`) into its
/// name and arity components.
fn parse_host_label(label: &str) -> (String, usize) {
    let rest = label
        .strip_prefix("host://")
        .or_else(|| label.strip_prefix(&format!("{HOST_SCHEME}://")))
        .unwrap_or(label);
    let rest = rest.split(" — ").next().unwrap_or(rest);
    split_host_uri(rest)
}

fn split_host_uri(rest: &str) -> (String, usize) {
    match rest.rsplit_once('/') {
        Some((name, arity)) => (name.to_string(), arity.parse().unwrap_or(0)),
        None => (rest.to_string(), 0),
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut catalog_path: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--catalog" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("rustscript-lsp: --catalog requires a file path");
                    std::process::exit(2);
                }
                catalog_path = Some(PathBuf::from(&args[i]));
            }
            "--help" | "-h" => {
                println!(
                    "rustscript-lsp — resource-aware RustScript language server (LSP over stdio)\n\n\
                     USAGE:\n    rustscript-lsp [--catalog <host-api-catalog.json>]\n\n\
                     Reads JSON-RPC messages from stdin, writes responses to stdout.\n\
                     --catalog loads a custom HostApiCatalog JSON snapshot (same serde shape the\n\
                     compiler validates); without it the standard sqlite+io+http catalog is used.\n"
                );
                return;
            }
            other => {
                eprintln!("rustscript-lsp: unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let catalog = match catalog_path {
        Some(path) => match load_catalog_file(&path) {
            Ok(catalog) => catalog,
            Err(message) => {
                eprintln!("rustscript-lsp: {message}");
                std::process::exit(3);
            }
        },
        None => standard_catalog(),
    };

    eprintln!(
        "rustscript-lsp: using host API catalog fingerprint {} ({} resources, {} functions)",
        catalog.fingerprint(),
        catalog.resources().len(),
        catalog.functions().len()
    );

    let mut server = LspServer::new(catalog);
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        let msg = match read_message(&mut reader) {
            ReadOutcome::Message(msg) => msg,
            ReadOutcome::Eof => {
                // Clean EOF: orderly exit. Per LSP, exiting without shutdown
                // is an error, but on EOF the client is gone; exit 1 only when
                // shutdown was never requested.
                if server.shutdown_requested {
                    std::process::exit(0);
                } else {
                    eprintln!("rustscript-lsp: EOF without shutdown");
                    std::process::exit(1);
                }
            }
            ReadOutcome::ParseError(message) => {
                // A recoverable malformed payload: respond with a JSON-RPC
                // parse error (-32700) and keep the server alive.
                eprintln!("rustscript-lsp: malformed payload: {message}");
                let response = error_message(
                    &serde_json::Value::Null,
                    -32700,
                    &format!("parse error: {message}"),
                );
                if let Err(err) = write_message(&mut out, &response) {
                    eprintln!("rustscript-lsp: failed writing response: {err}");
                    std::process::exit(1);
                }
                continue;
            }
            ReadOutcome::Fatal(message) => {
                eprintln!("rustscript-lsp: framing error: {message}");
                std::process::exit(1);
            }
        };

        // exit is a notification: terminate after processing.
        let is_exit = msg.method == "exit";
        let response = match server.handle(&msg, &mut out) {
            Ok(response) => response,
            Err(err) => {
                eprintln!("rustscript-lsp: io error: {err}");
                std::process::exit(1);
            }
        };
        if let Some(response) = response {
            if let Err(err) = write_message(&mut out, &response) {
                eprintln!("rustscript-lsp: failed writing response: {err}");
                std::process::exit(1);
            }
        }
        if is_exit {
            if server.shutdown_requested {
                std::process::exit(0);
            } else {
                std::process::exit(1);
            }
        }
    }
}
