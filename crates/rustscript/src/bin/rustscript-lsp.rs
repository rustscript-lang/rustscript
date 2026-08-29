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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustscript::{
    CompileSourceFileOptions, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostTypeSchema,
    ParseError, SemanticDiagnostic, SemanticModel, SourceError, SourceMap, SourcePathError,
    SourcePosition, Span, analyze_source_from_string_with_options,
};

/// Hard cap on a single JSON-RPC message payload (LSP bodies are small; a
/// pathological client cannot exhaust memory).
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Hard cap on an individual document's text (editors can send huge buffers;
/// bound reanalysis cost).
const MAX_DOCUMENT_CHARS: usize = 8 * 1024 * 1024;
/// Hard cap on a single header line. LSP headers are a few hundred bytes; a
/// pathological client must not be able to force unbounded allocation before
/// `Content-Length` is even parsed.
const MAX_HEADER_LINE_BYTES: usize = 16 * 1024;
/// Hard cap on the cumulative header block of one message.
const MAX_HEADER_TOTAL_BYTES: usize = 64 * 1024;
/// Scheme used for virtual host-definition documents.
const HOST_SCHEME: &str = "rustscript-host";

/// Runtime-tunable robustness caps. Production defaults are the constants
/// above; tests may lower them via CLI flags to exercise the guard paths
/// without transferring multi-megabyte payloads.
#[derive(Clone, Copy, Debug)]
struct ServerConfig {
    max_message_bytes: usize,
    max_document_chars: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: MAX_MESSAGE_BYTES,
            max_document_chars: MAX_DOCUMENT_CHARS,
        }
    }
}

/// A JSON-RPC message with an optional id (notifications omit it).
#[derive(Debug, Clone)]
struct RpcMessage {
    id: Option<serde_json::Value>,
    method: String,
    params: serde_json::Value,
}

/// The outcome of reading one message: a parsed message, or a recoverable
/// parse error to respond with (`-32700`), or a fatal framing error.
#[derive(Debug)]
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

#[derive(Debug, PartialEq, Eq)]
enum HeaderLine {
    /// A complete line without its LF/CRLF delimiter.
    Line { len: usize, bytes_read: usize },
    /// EOF before any byte of the next line.
    Eof,
}

/// Read one header line without allowing the reader to grow an unbounded
/// `String`. The caller supplies the fixed-size scratch buffer, so the only
/// state retained between chunks is at most `MAX_HEADER_LINE_BYTES` bytes plus
/// the delimiter bookkeeping.
fn read_header_line(
    reader: &mut impl BufRead,
    buffer: &mut [u8; MAX_HEADER_LINE_BYTES],
) -> Result<HeaderLine, &'static str> {
    let mut len = 0usize;
    let mut bytes_read = 0usize;
    loop {
        let available = reader.fill_buf().map_err(|_| "failed reading header")?;
        if available.is_empty() {
            return if len == 0 {
                Ok(HeaderLine::Eof)
            } else {
                Err("unexpected EOF inside message headers")
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(available.len());
        if len.saturating_add(data_len) > MAX_HEADER_LINE_BYTES {
            return Err("header line exceeds the size cap");
        }
        buffer[len..len + data_len].copy_from_slice(&available[..data_len]);
        len += data_len;

        let consumed = newline.map_or(data_len, |index| index + 1);
        reader.consume(consumed);
        bytes_read += consumed;
        if newline.is_some() {
            // Accept both LF and CRLF while keeping a bare CR in the header
            // content (it is not a delimiter on its own).
            if buffer[..len].last() == Some(&b'\r') {
                len -= 1;
            }
            return Ok(HeaderLine::Line { len, bytes_read });
        }
    }
}

/// Parse a `Content-Length` framed JSON-RPC message from a reader.
///
/// Returns [`ReadOutcome::Message`] on success, [`ReadOutcome::Eof`] on
/// clean EOF before any header, [`ReadOutcome::ParseError`] for malformed
/// JSON bodies (recoverable), and [`ReadOutcome::Fatal`] for broken framing
/// or over-limit payloads (the stream cannot be resynced).
fn read_message(reader: &mut impl BufRead, max_message_bytes: usize) -> ReadOutcome {
    let mut content_length: Option<usize> = None;
    let mut header_total = 0usize;
    let mut header_buffer = [0u8; MAX_HEADER_LINE_BYTES];
    loop {
        let line = match read_header_line(reader, &mut header_buffer) {
            Ok(line) => line,
            Err(message) => return ReadOutcome::Fatal(message.to_string()),
        };
        let HeaderLine::Line { len, bytes_read } = line else {
            return if header_total == 0 {
                ReadOutcome::Eof
            } else {
                ReadOutcome::Fatal("unexpected EOF inside message headers".to_string())
            };
        };
        header_total = match header_total.checked_add(bytes_read) {
            Some(total) if total <= MAX_HEADER_TOTAL_BYTES => total,
            _ => {
                return ReadOutcome::Fatal("message headers exceed the size cap".to_string());
            }
        };
        if len == 0 {
            break;
        }

        let line = match std::str::from_utf8(&header_buffer[..len]) {
            Ok(line) => line,
            Err(_) => return ReadOutcome::Fatal("message header is not valid UTF-8".to_string()),
        };
        let Some((name, value)) = line.split_once(':') else {
            return ReadOutcome::Fatal("malformed message header".to_string());
        };
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return ReadOutcome::Fatal("duplicate Content-Length header".to_string());
            }
            let parsed: usize = match value.trim().parse() {
                Ok(parsed) => parsed,
                Err(_) => return ReadOutcome::Fatal("invalid Content-Length header".to_string()),
            };
            if parsed > max_message_bytes {
                return ReadOutcome::Fatal("message exceeds the size cap".to_string());
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

/// Build the standard host catalog used by the compiler and language server.
fn standard_catalog() -> Arc<HostApiCatalog> {
    rustscript::standard_host_catalog()
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

/// Return the byte offset before the line terminator, if this chunk has one.
fn line_content_end(text: &str, line_start: usize, chunk: &str) -> usize {
    let mut content_end = line_start + chunk.len();
    if chunk.ends_with('\n') {
        content_end -= 1;
        if content_end > line_start && text.as_bytes()[content_end - 1] == b'\r' {
            content_end -= 1;
        }
    }
    content_end
}

/// Convert an LSP `Position` (0-indexed line, UTF-16 code-unit column) to a
/// byte offset within `text`. Returns `None` for out-of-range positions and
/// positions inside a UTF-16 surrogate pair.
fn lsp_position_to_offset(text: &str, line: u32, character: u32) -> Option<usize> {
    let mut line_start = 0usize;
    for (line_index, chunk) in text.split_inclusive('\n').enumerate() {
        if line_index == line as usize {
            let content_end = line_content_end(text, line_start, chunk);
            let line_text = &text[line_start..content_end];
            let mut utf16_seen = 0u32;
            for (byte_idx, ch) in line_text.char_indices() {
                if character == utf16_seen {
                    return Some(line_start + byte_idx);
                }
                let next = utf16_seen.saturating_add(ch.len_utf16() as u32);
                if character < next {
                    // There is no UTF-8 byte offset for the interior of a
                    // supplementary-plane scalar.
                    return None;
                }
                utf16_seen = next;
            }
            return (character == utf16_seen).then_some(content_end);
        }
        line_start += chunk.len();
    }

    // A trailing newline creates one valid empty line at EOF. Empty text is
    // the equivalent single empty line.
    let line_count = text.split_inclusive('\n').count() as u32;
    if text.is_empty() || (text.ends_with('\n') && line == line_count) {
        Some(text.len())
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
    for chunk in text.split_inclusive('\n') {
        let chunk_end = line_start + chunk.len();
        let content_end = line_content_end(text, line_start, chunk);
        if offset <= content_end {
            let utf16 = text[line_start..offset]
                .chars()
                .map(|ch| ch.len_utf16() as u32)
                .sum();
            return Some((line, utf16));
        }
        if offset < chunk_end {
            // Offsets in CRLF/LF are represented by the preceding line's end
            // position; the byte after LF belongs to the next line.
            let utf16 = text[line_start..content_end]
                .chars()
                .map(|ch| ch.len_utf16() as u32)
                .sum();
            return Some((line, utf16));
        }
        line_start = chunk_end;
        line += 1;
    }

    // Offset at EOF after a final newline is the start of the trailing empty
    // line. For an empty source this also returns (0, 0).
    Some((line, 0))
}

// ---------------------------------------------------------------------------
// Document store
// ---------------------------------------------------------------------------

/// One open document: its URI, its canonical module identity (see
/// [`canonical_identity`]), the current buffer text, and the last analysis
/// result (if any).
struct Document {
    uri: String,
    /// Canonical module identity — the exact path form the compiler's loader
    /// records in the SourceMap and resolves imported modules to.
    identity: PathBuf,
    text: String,
    model: Option<SemanticModel>,
    /// The canonical identities of every file module in the last successful
    /// compilation, excluding this document. Keeping the resolved closure
    /// means a change to a transitive dependency can invalidate this importer
    /// without reparsing import syntax in the LSP layer.
    dependencies: BTreeSet<PathBuf>,
    /// Failed module loading leaves no complete graph. Unknown documents are
    /// rechecked on the next document event so a newly opened virtual or
    /// previously missing dependency cannot leave a stale model behind.
    dependencies_known: bool,
    /// Rendered parse/load diagnostics from a failed analysis, keyed by owning
    /// URI. Present only when the most recent analysis failed; cleared on
    /// success. See [`render_analysis_error`].
    analysis_error: Option<std::collections::BTreeMap<String, Vec<serde_json::Value>>>,
}

impl Document {
    fn new(uri: String, identity: PathBuf, text: String) -> Self {
        Self {
            uri,
            identity,
            text,
            model: None,
            dependencies: BTreeSet::new(),
            dependencies_known: false,
            analysis_error: None,
        }
    }
}

/// Convert an LSP document URI to a canonical module identity. Supports
/// `file://` URIs (percent-decoded); other schemes map to a synthetic
/// in-memory identity rooted under the host scheme so virtual host documents
/// stay addressable.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path_str = percent_decode(rest);
    Some(PathBuf::from(path_str))
}

/// The single canonical identity for a document/module path, shared across
/// every path form in this server:
///
/// * LSP URI→path (`uri_to_path` / `Document::path`),
/// * `compile_options` module-override keys,
/// * `SourceMap` source-name→URI lookup (`uri_for_source_name`),
/// * source-id resolution against an open document (`source_position`), and
/// * closed-source suppression (`closed_doc_source`).
///
/// It deliberately mirrors the compiler's `module_identity` (the loader's
/// canonical identity) so override keys registered here match the resolved
/// path string the loader looks up: a path that exists on disk canonicalizes
/// to its absolute canonical path, while an unsaved/nonexistent buffer keeps
/// a normalized absolute path so virtual buffers and the importers that
/// depend on them agree on identity deterministically. Relative path forms
/// (e.g. percent-decoded URIs without a leading slash) are anchored to the
/// current directory first so the resulting identity is always absolute,
/// which is exactly the offset the loader produces for the same path.
fn canonical_identity(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    if absolute.is_file()
        && let Ok(canonical) = absolute.canonicalize()
    {
        return canonical;
    }
    normalize_absolute_path(&absolute)
}

/// Lexically normalize an absolute path (resolve `.` and `..`), preserving
/// the leading root. Mirrors the loader's `normalize_module_path`.
fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // Never escape the root or drop leading parent segments on an
                // absolute path (they cannot legally exist above the root).
                Some(Component::ParentDir)
                | Some(Component::RootDir | Component::Prefix(_))
                | Some(Component::CurDir)
                | None => {}
            },
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

/// The slash-normalized string form of a canonical identity, used as the key
/// throughout the server's path maps (backslashes to slashes so Windows-style
/// and Unix-style names compare equal).
fn normalized_source_name(name: &str) -> String {
    name.replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Analysis-error rendering
// ---------------------------------------------------------------------------

/// Render a failed `analyze_source_from_string_with_options` into an LSP
/// publishDiagnostics payload grouped by owning URI.
///
/// Source errors (parse/load failures) carry a [`SourceMap`] (via
/// `SourcePathError::SourceWithMap`) that resolves every span they reference
/// to its owning source text, and the span itself identifies the owning
/// source id. We therefore render an exact, source-accurate diagnostic rather
/// than dead-lettering the failure. A bare `Source(ParseError)` without a map
/// is rendered against the entry document's own identity/text at the line the
/// parser reported. Other path-level errors (unreadable import etc.) are
/// rendered as a single line-1 diagnostic on the entry document.
///
/// The returned map is keyed by the owning client URI (canonical identity →
/// `uri_for_source_name`), so the error renders against the module URI that
/// actually failed — an imported module's syntax error is attributed to that
/// module, never the importer.
fn render_analysis_error(
    server: &LspServer,
    err: &SourcePathError,
    entry_identity: &Path,
) -> std::collections::BTreeMap<String, Vec<serde_json::Value>> {
    let mut out = std::collections::BTreeMap::new();
    match err {
        SourcePathError::SourceWithMap { error, sources } => match error {
            SourceError::Parse(parse) => {
                push_parse_diagnostic(server, &mut out, sources, entry_identity, parse);
            }
            SourceError::Compile(compile) => {
                // A compile error carried as a source-path failure (e.g. a
                // resolving/legalize failure surfaced through the loader).
                // Resolve its carried span against the attached map.
                let (name, text, lo, hi) = match compile_span(compile) {
                    Some(span) => match sources.file(span.source_id) {
                        Some(file) => (
                            file.name.clone(),
                            file.text.clone(),
                            span.lo.min(file.text.len()),
                            span.hi.min(file.text.len()),
                        ),
                        None => (entry_identity.display().to_string(), String::new(), 0, 0),
                    },
                    None => (entry_identity.display().to_string(), String::new(), 0, 0),
                };
                let uri = server.uri_for_source_name(&name);
                push_diag(
                    &mut out,
                    uri,
                    &text,
                    lo,
                    hi,
                    compile.diagnostic_message(),
                    Some("E101".to_string()),
                );
            }
        },
        SourcePathError::Source(SourceError::Parse(parse)) => {
            // No source map attached: render against the entry document using
            // its own identity and a source map containing just the entry.
            let mut source_map = SourceMap::new();
            let entry_text = std::fs::read_to_string(entry_identity).unwrap_or_default();
            let id =
                source_map.add_source(entry_identity.display().to_string(), entry_text.clone());
            let mut parse = parse.clone();
            if parse.span.is_none() {
                parse = parse.with_line_span_from_source(&source_map, id);
            }
            push_parse_diagnostic(server, &mut out, &source_map, entry_identity, &parse);
        }
        SourcePathError::Source(SourceError::Compile(compile)) => {
            let uri = server.uri_for_source_name(&entry_identity.display().to_string());
            push_diag(
                &mut out,
                uri,
                "",
                0,
                0,
                compile.diagnostic_message(),
                Some("E101".to_string()),
            );
        }
        // Path-level failure (Io, import cycle, missing extension, invalid
        // import syntax, ...): report on the entry document's first line.
        other => {
            let uri = server.uri_for_source_name(&entry_identity.display().to_string());
            push_diag(
                &mut out,
                uri,
                "",
                0,
                0,
                other.to_string(),
                Some("E100".to_string()),
            );
        }
    }
    out
}

/// The compile span carried by a `CompileError`, if any.
fn compile_span(compile: &rustscript::CompileError) -> Option<Span> {
    match compile {
        rustscript::CompileError::HostCallResolve { span, .. }
        | rustscript::CompileError::IfElseBranchTypeMismatch { span, .. }
        | rustscript::CompileError::CallableArgumentTypeMismatch { span, .. }
        | rustscript::CompileError::BinaryOperandTypeMismatch { span, .. }
        | rustscript::CompileError::InvalidFieldAccess { span, .. }
        | rustscript::CompileError::FunctionParameterTypeConflict { span, .. }
        | rustscript::CompileError::StrictTypingRequired { span, .. } => *span,
        _ => None,
    }
}

/// Keep zero-width parser errors at EOF attached to the final source line.
/// The generic position conversion intentionally exposes the trailing empty
/// line, but an unterminated construct's diagnostic belongs to its opening
/// line for editor highlighting.
fn parse_diagnostic_span(text: &str, lo: usize, hi: usize) -> (usize, usize) {
    if lo == text.len() && hi == text.len() && text.ends_with('\n') {
        let mut end = text.len() - 1;
        if end > 0 && text.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        (end, end)
    } else {
        (lo, hi)
    }
}

/// Render a [`ParseError`] diagnostic, resolving its span against the
/// attached SourceMap and attaching it to the owning source's URI.
fn push_parse_diagnostic(
    server: &LspServer,
    out: &mut std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    sources: &SourceMap,
    entry_identity: &Path,
    parse: &ParseError,
) {
    let (name, text, lo, hi) = match parse.span {
        Some(span) => match sources.file(span.source_id) {
            Some(file) => (
                file.name.clone(),
                file.text.clone(),
                span.lo.min(file.text.len()),
                span.hi.min(file.text.len()),
            ),
            None => (entry_identity.display().to_string(), String::new(), 0, 0),
        },
        None => (entry_identity.display().to_string(), String::new(), 0, 0),
    };
    let uri = server.uri_for_source_name(&name);
    let (lo, hi) = parse_diagnostic_span(text.as_str(), lo, hi);
    push_diag(
        out,
        uri,
        &text,
        lo,
        hi,
        parse.message.clone(),
        parse.code.clone(),
    );
}

/// Push one LSP diagnostic value into the grouped map.
fn push_diag(
    out: &mut std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    uri: String,
    text: &str,
    lo: usize,
    hi: usize,
    message: String,
    code: Option<String>,
) {
    let (start, end) = span_to_lsp(text, lo, hi);
    let mut diag = serde_json::json!({
        "range": { "start": { "line": start.0, "character": start.1 },
                   "end": { "line": end.0, "character": end.1 } },
        "severity": 1,
        "source": "rustscript",
        "message": message,
    });
    if let Some(code) = code {
        diag["code"] = serde_json::Value::String(code);
    }
    out.entry(uri).or_default().push(diag);
}

/// Convert a byte span to an LSP range against a source text.
fn span_to_lsp(text: &str, lo: usize, hi: usize) -> ((u32, u32), (u32, u32)) {
    let lo = lo.min(text.len());
    let hi = hi.min(text.len());
    let start = offset_to_lsp_position(text, lo).unwrap_or((0, 0));
    let end = offset_to_lsp_position(text, hi).unwrap_or(start);
    (start, end)
}

/// Minimal percent-decoding for URI paths (LSP file URIs percent-encode
/// spaces and non-ASCII).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
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
    documents: BTreeMap<String, Document>,
    /// Canonical dependency identity -> open importer URIs. Both keys and
    /// values are ordered so invalidation and subsequent analysis are
    /// deterministic.
    dependents: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Every URI we have published diagnostics for (including URIs owned by
    /// imported modules of an analyzed document). On reanalysis/change/close
    /// any URI that drops out of the fresh diagnostic set is cleared with an
    /// empty publish so the client never shows stale squiggles.
    published_uris: std::collections::HashSet<String>,
    /// Canonical module identities (slash-normalized) whose documents have
    /// been closed and must not contribute diagnostics until reopened/reloaded
    /// from disk (the closing document's buffer is gone, so its errors must be
    /// cleared even if a still-open importing document's model still
    /// references them).
    closed_sources: std::collections::HashSet<String>,
    shutdown_requested: bool,
    initialized: bool,
    config: ServerConfig,
}

impl LspServer {
    fn new(catalog: Arc<HostApiCatalog>, config: ServerConfig) -> Self {
        Self {
            catalog,
            documents: BTreeMap::new(),
            dependents: BTreeMap::new(),
            published_uris: std::collections::HashSet::new(),
            closed_sources: std::collections::HashSet::new(),
            shutdown_requested: false,
            initialized: false,
            config,
        }
    }

    fn remove_document_dependencies(&mut self, uri: &str) {
        let Some(dependencies) = self.documents.get_mut(uri).map(|doc| {
            doc.dependencies_known = false;
            std::mem::take(&mut doc.dependencies)
        }) else {
            return;
        };
        for dependency in dependencies {
            let remove_key = if let Some(importers) = self.dependents.get_mut(&dependency) {
                importers.remove(uri);
                importers.is_empty()
            } else {
                false
            };
            if remove_key {
                self.dependents.remove(&dependency);
            }
        }
    }

    fn remove_document(&mut self, uri: &str) -> Option<Document> {
        self.remove_document_dependencies(uri);
        self.documents.remove(uri)
    }

    fn set_document_dependencies(&mut self, uri: &str, dependencies: BTreeSet<PathBuf>) {
        self.remove_document_dependencies(uri);
        if let Some(doc) = self.documents.get_mut(uri) {
            doc.dependencies = dependencies.clone();
            doc.dependencies_known = true;
        } else {
            return;
        }
        for dependency in dependencies {
            self.dependents
                .entry(dependency)
                .or_default()
                .insert(uri.to_string());
        }
    }

    fn module_dependencies(
        model: &SemanticModel,
        entry_identity: &Path,
    ) -> Option<BTreeSet<PathBuf>> {
        let graph = model.module_graph()?;
        let mut dependencies = BTreeSet::new();
        for node in graph.nodes() {
            let identity = canonical_identity(&node.identity);
            if identity != entry_identity {
                dependencies.insert(identity);
            }
        }
        Some(dependencies)
    }

    /// Find all open importers of the changed identities, walking the reverse
    /// graph with a visited set. The stored dependency sets are transitive
    /// closures, so this also works when an intermediate module is not open.
    fn dependent_documents_for(&self, changed: &[PathBuf]) -> BTreeSet<String> {
        let mut queue = VecDeque::from(changed.to_vec());
        let mut visited = BTreeSet::new();
        let mut affected = BTreeSet::new();
        while let Some(identity) = queue.pop_front() {
            if !visited.insert(identity.clone()) {
                continue;
            }
            let Some(importers) = self.dependents.get(&identity) else {
                continue;
            };
            for importer in importers {
                if affected.insert(importer.clone())
                    && let Some(doc) = self.documents.get(importer)
                {
                    queue.push_back(doc.identity.clone());
                }
            }
        }
        // A failed load has no complete graph. Rechecking these documents on
        // every event closes the gap for newly opened or deleted virtual files.
        for (uri, doc) in &self.documents {
            if !doc.dependencies_known {
                affected.insert(uri.clone());
            }
        }
        affected
    }

    fn reanalyze_documents(&mut self, uris: BTreeSet<String>) {
        for uri in uris {
            self.analyze_document(&uri);
        }
    }

    /// The compile options for this server: the exact catalog snapshot plus
    /// module-source overrides for every open document (so an open buffer
    /// shadows the on-disk module it corresponds to).
    ///
    /// Overrides are keyed by the document's canonical module identity — the
    /// exact path form the loader resolves imported modules to and records in
    /// the SourceMap (see [`canonical_identity`]). Bare basename aliases are
    /// deliberately *not* registered: two open documents may share a basename
    /// in different directories, and an unconditional basename override would
    /// make one nondeterministically shadow the other. Ambiguous basenames
    /// are instead left to the resolved-identity lookup, which is exact.
    fn compile_options(&self) -> CompileSourceFileOptions {
        let mut options =
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&self.catalog));
        for doc in self.documents.values() {
            let spec = normalized_source_name(&doc.identity.to_string_lossy());
            options = options.with_module_override_source(spec, doc.text.clone());
        }
        options
    }

    /// Analyze (or reanalyze) the document at `uri` with its current buffer
    /// text. Stale diagnostics for other documents are cleared by the caller.
    ///
    /// On analysis failure the previous model is dropped (never retained
    /// against changed text) and the failure is recorded so the caller can
    /// publish exact parse/load diagnostics from the error's attached
    /// [`SourceMap`] instead of dead-lettering them.
    fn analyze_document(&mut self, uri: &str) {
        let options = self.compile_options();
        let Some((identity, text)) = self
            .documents
            .get(uri)
            .map(|doc| (doc.identity.clone(), doc.text.clone()))
        else {
            return;
        };
        // Remove the previous graph before rebuilding. On failure this leaves
        // the document explicitly unknown, so later dependency events trigger
        // a conservative refresh instead of using stale provenance.
        self.remove_document_dependencies(uri);
        let result = analyze_source_from_string_with_options(&identity, &text, options);
        match result {
            Ok(model) => {
                let dependencies = Self::module_dependencies(&model, &identity);
                if let Some(doc) = self.documents.get_mut(uri) {
                    doc.model = Some(model);
                    doc.analysis_error = None;
                }
                if let Some(dependencies) = dependencies {
                    self.set_document_dependencies(uri, dependencies);
                }
            }
            Err(err) => {
                // Never retain a stale model against changed text: the buffer
                // no longer parses/loads, so every query against the old model
                // would be wrong. Render the failure as exact parse/load
                // diagnostics from the error's attached source map (this needs
                // an immutable borrow of `self` for URI resolution, so the
                // entry document's mutable borrow ends before the render).
                let rendered = render_analysis_error(self, &err, &identity);
                let Some(doc) = self.documents.get_mut(uri) else {
                    return;
                };
                doc.model = None;
                doc.analysis_error = Some(rendered);
            }
        }
    }

    /// Map a SourceMap file name back to a client URI. Open documents map to
    /// their document URI (matched by canonical identity); other real files
    /// map to a canonical `file://` URI; host/virtual names map to the host
    /// scheme (never double-prefixed).
    fn uri_for_source_name(&self, name: &str) -> String {
        // The SourceMap records canonical identities (the loader's path
        // form); match each document's canonical identity string exactly.
        let canonical = canonical_identity(Path::new(name));
        let canonical_str = normalized_source_name(&canonical.to_string_lossy());
        for doc in self.documents.values() {
            let doc_identity = normalized_source_name(&doc.identity.to_string_lossy());
            if doc_identity == canonical_str {
                return doc.uri.clone();
            }
        }
        if let Some(rest) = name.strip_prefix("host://") {
            return format!("{HOST_SCHEME}://{rest}");
        }
        if let Some(rest) = name.strip_prefix(&format!("{HOST_SCHEME}://")) {
            return format!("{HOST_SCHEME}://{rest}");
        }
        if name.starts_with(HOST_SCHEME) {
            // Already a host-scheme URI (e.g. `rustscript-host://foo/1`);
            // never stack another scheme prefix.
            return name.to_string();
        }
        // A real file path: emit a canonical file URI from the canonical
        // identity so the client can navigate to disk-provided modules.
        if canonical.is_absolute() {
            format!("file://{}", canonical_str)
        } else {
            format!("file://{}", name)
        }
    }

    /// Collect every diagnostic across all open documents' models, grouped by
    /// the owning URI (resolved through each diagnostic's `span.source_id`).
    /// The entry URI of every open document is always present (empty array
    /// when its analysis produced nothing), so clean documents still publish
    /// an explicit clear. Failed analyses contribute their rendered
    /// parse/load diagnostics (see [`render_analysis_error`]).
    fn diagnostics_grouped_by_uri(
        &self,
    ) -> std::collections::BTreeMap<String, Vec<serde_json::Value>> {
        let mut grouped: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
            std::collections::BTreeMap::new();
        // Every open document owns its entry URI, even with zero diagnostics.
        for doc in self.documents.values() {
            grouped.entry(doc.uri.clone()).or_default();
        }
        // A module owned by several open documents' models (its own model and
        // every graph that imports it) surfaces the same diagnostic more than
        // once; deduplicate by URI + rendered range + message + code so the
        // client sees each squiggle exactly once (the source_id is
        // model-relative, so it cannot be part of the key).
        let mut seen: std::collections::HashSet<(String, String, String, String)> =
            std::collections::HashSet::new();
        for doc in self.documents.values() {
            let Some(model) = &doc.model else {
                continue;
            };
            for diag in model.diagnostics() {
                let Some((uri, value)) = self.lsp_diagnostic(model, &diag) else {
                    continue;
                };
                let key = (
                    uri.clone(),
                    diag.message.clone(),
                    diag.code.clone().unwrap_or_default(),
                    value["range"].to_string(),
                );
                if seen.insert(key) {
                    grouped.entry(uri).or_default().push(value);
                }
            }
        }
        // Failed analyses: their rendered diagnostics already carry exact
        // ranges and owning URIs; fold them into the grouped set.
        for doc in self.documents.values() {
            if let Some(error_diags) = &doc.analysis_error {
                for (uri, diags) in error_diags {
                    let entry = grouped.entry(uri.clone()).or_default();
                    for diag in diags {
                        let key = (
                            uri.clone(),
                            diag["message"].as_str().unwrap_or("").to_string(),
                            diag["code"].as_str().unwrap_or("").to_string(),
                            diag["range"].to_string(),
                        );
                        if seen.insert(key) {
                            entry.push(diag.clone());
                        }
                    }
                }
            }
        }
        grouped
    }

    fn lsp_diagnostic(
        &self,
        model: &SemanticModel,
        diag: &SemanticDiagnostic,
    ) -> Option<(String, serde_json::Value)> {
        let span = diag.span?;
        // Resolve the diagnostic's owning source through the SourceMap. Every
        // span the linker carries references its own module's graph SourceId,
        // so offsets are only meaningful against the owning source's text.
        let owning = model.sources().file(span.source_id);
        let (name, text) = match owning {
            Some(file) => (file.name.as_str(), file.text.as_str()),
            None => {
                // Unknown source id: fall back to the entry document's URI
                // and text so a diagnostic is still surfaced.
                match self.documents.values().find(|doc| doc.model.is_some()) {
                    Some(entry) => (entry.uri.as_str(), entry.text.as_str()),
                    None => return None,
                }
            }
        };
        let uri = self.uri_for_source_name(name);
        // A document that was closed must not keep contributing diagnostics
        // through another open document's stale model. The suppression key is
        // the canonical identity, matching `closed_doc_source` exactly.
        let name_identity =
            normalized_source_name(&canonical_identity(Path::new(name)).to_string_lossy());
        if self.closed_sources.contains(&name_identity)
            && !self.documents.values().any(|doc| doc.uri == uri)
        {
            return None;
        }
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
        Some((uri, value))
    }

    /// Publish diagnostics for every open document (grouped by owning URI),
    /// clearing any previously published URI that no longer owns diagnostics.
    /// After publishing, the tracked published set is the fresh URI set, so
    /// the next publish clears anything that drops out.
    fn publish_all_diagnostics(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let grouped = self.diagnostics_grouped_by_uri();
        // Clear stale: every URI we have ever published for that is not part
        // of the fresh set (e.g. an imported module that no longer produces
        // diagnostics, or a closed document) gets an empty publish.
        let fresh: std::collections::HashSet<String> = grouped.keys().cloned().collect();
        let mut to_publish: Vec<(String, Vec<serde_json::Value>)> = grouped.into_iter().collect();
        for stale in self.published_uris.difference(&fresh) {
            to_publish.push((stale.clone(), Vec::new()));
        }
        to_publish.sort_by(|a, b| a.0.cmp(&b.0));
        for (uri, diagnostics) in to_publish {
            let params = serde_json::json!({
                "uri": uri,
                "diagnostics": diagnostics,
            });
            write_message(
                out,
                &serde_json::json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params }),
            )?;
        }
        self.published_uris = fresh;
        Ok(())
    }

    /// Drop an oversized document and publish an explicit empty diagnostic
    /// array for its URI so the client clears any previously shown squiggles.
    fn reject_oversized_document(
        &mut self,
        out: &mut impl Write,
        uri: &str,
    ) -> std::io::Result<()> {
        let changed = self
            .documents
            .get(uri)
            .map(|doc| vec![doc.identity.clone()])
            .unwrap_or_default();
        let affected = self.dependent_documents_for(&changed);
        self.remove_document(uri);
        self.published_uris.insert(uri.to_string());
        self.reanalyze_documents(affected);
        self.publish_all_diagnostics(out)
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
        // The SourceMap records canonical identities, so look up the
        // document's canonical identity exactly; only when the document is
        // not present in the map at all (e.g. an imported module whose buffer
        // shadows a different on-disk file) fall back to a deterministic
        // suffix match.
        let identity_str = normalized_source_name(&doc.identity.to_string_lossy());
        let source_id = model
            .sources()
            .source_id_by_name(&identity_str)
            .or_else(|| {
                // Fall back to the first source whose canonical identity ends
                // with this document's file name.
                let file_name = doc.identity.file_name()?.to_str()?;
                let file_name = normalized_source_name(file_name);
                let mut found = None;
                for id in 0.. {
                    let Some(name) = model.sources().file_name(id) else {
                        break;
                    };
                    let canonical = normalized_source_name(
                        &canonical_identity(Path::new(name)).to_string_lossy(),
                    );
                    if canonical.ends_with(&file_name) {
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
        // ---- lifecycle enforcement ----
        if self.shutdown_requested {
            // After shutdown only `exit` is serviced; every other request is
            // rejected with InvalidRequest per the LSP spec.
            if method == "exit" {
                return Ok(None);
            }
            if let Some(id) = msg.id.as_ref() {
                return Ok(Some(error_message(id, -32600, "server is shutting down")));
            }
            // Notifications after shutdown are dropped per spec.
            return Ok(None);
        }
        if !self.initialized && method != "initialize" {
            // Requests before initialize are rejected with
            // ServerNotInitialized; notifications are dropped.
            if let Some(id) = msg.id.as_ref() {
                return Ok(Some(error_message(id, -32002, "server not initialized")));
            }
            return Ok(None);
        }
        match method {
            // ---- lifecycle ----
            "initialize" => {
                // Per the LSP spec a second initialize (after the first
                // succeeded) is an error: the server is already initialized.
                // Respond InvalidRequest so clients detect the duplicate.
                if self.initialized {
                    return Ok(Some(error_message(
                        msg.id.as_ref().unwrap_or(&serde_json::Value::Null),
                        -32600,
                        "server is already initialized",
                    )));
                }
                self.initialized = true;
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
                if let Some(id) = msg.id.as_ref() {
                    Ok(Some(error_message(
                        id,
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
        if text.chars().count() > self.config.max_document_chars {
            // Oversized buffer: drop the document (do not analyze). Clear any
            // diagnostics that were previously published for it.
            return self.reject_oversized_document(out, &uri);
        }
        let path =
            uri_to_path(&uri).unwrap_or_else(|| PathBuf::from(uri.trim_start_matches("file://")));
        let identity = canonical_identity(&path);
        let mut changed = vec![identity.clone()];
        if let Some(previous) = self.documents.get(&uri) {
            changed.push(previous.identity.clone());
        }
        let affected = self.dependent_documents_for(&changed);
        self.remove_document(&uri);
        // Reopened: the buffer is live again, so its source may contribute
        // diagnostics and module overrides.
        self.closed_sources
            .remove(&normalized_source_name(&identity.to_string_lossy()));
        self.documents.insert(
            uri.clone(),
            Document::new(uri.clone(), identity, text.to_string()),
        );
        let mut to_reanalyze = affected;
        to_reanalyze.insert(uri.clone());
        self.reanalyze_documents(to_reanalyze);
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
        if text.chars().count() > self.config.max_document_chars {
            // Oversized replacement: drop the document and clear its
            // diagnostics (the buffer cannot be analyzed).
            return self.reject_oversized_document(out, &uri);
        }
        let Some(identity) = self.documents.get(&uri).map(|doc| doc.identity.clone()) else {
            return Ok(());
        };
        let changed = [identity];
        let affected = self.dependent_documents_for(&changed);
        if let Some(doc) = self.documents.get_mut(&uri) {
            doc.text = text.to_string();
        }
        let mut to_reanalyze = affected;
        to_reanalyze.insert(uri.clone());
        self.reanalyze_documents(to_reanalyze);
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
        let changed = self
            .documents
            .get(&uri)
            .map(|doc| vec![doc.identity.clone()])
            .unwrap_or_default();
        let mut affected = self.dependent_documents_for(&changed);
        let removed = self.remove_document(&uri);
        // Remember this source as closed so stale diagnostics from still-open
        // importing documents' models are not reported for it.
        if let Some(doc) = removed {
            self.closed_sources
                .insert(normalized_source_name(&doc.identity.to_string_lossy()));
        } else if let Some(identity) = self.closed_doc_source(&uri) {
            self.closed_sources.insert(identity);
        }
        affected.remove(&uri);
        self.reanalyze_documents(affected);
        // The closed document's URI (and any module URIs it published for)
        // drops out of the fresh diagnostic set and is cleared by
        // `publish_all_diagnostics`.
        self.publish_all_diagnostics(out)
    }

    /// The canonical source identity a document URI used, for closed-source
    /// tracking. Mirrors `canonical_identity` so the key matches what the
    /// SourceMap records for the same document and what `lsp_diagnostic`
    /// compares against.
    fn closed_doc_source(&self, uri: &str) -> Option<String> {
        let rest = uri.strip_prefix("file://")?;
        let path_str = percent_decode(rest);
        let identity = canonical_identity(Path::new(&path_str));
        Some(normalized_source_name(&identity.to_string_lossy()))
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
                    // and exact overload discriminator so the location is
                    // stable and overload-specific.
                    let (name, arity, discriminator) = parse_host_label(&def.label);
                    let host_uri = match discriminator.as_deref() {
                        Some(discriminator) => {
                            format!("{HOST_SCHEME}://{name}/{arity}/{discriminator}")
                        }
                        None => format!("{HOST_SCHEME}://{name}/{arity}"),
                    };
                    // The location must identify the actual rendered function
                    // entry (the signature line) in the virtual document, not
                    // a zero-width placeholder. When several catalog entries
                    // share name+arity the line is deterministic per function.
                    let range = self.host_entry_range(&name, arity, discriminator.as_deref());
                    return serde_json::json!([{
                        "uri": host_uri,
                        "range": range,
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
        let (name, arity, discriminator) = split_host_uri(rest);
        let all_matches: Vec<&HostFunctionSchema> = self
            .catalog
            .functions()
            .iter()
            .filter(|f| f.name == name)
            .filter(|f| f.params.len() == arity)
            .collect();
        let matches: Vec<&HostFunctionSchema> = match discriminator.as_deref() {
            Some(discriminator) => all_matches
                .into_iter()
                .filter(|function| function.identity_discriminator() == discriminator)
                .collect(),
            None => all_matches,
        };
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

    /// The LSP range of the rendered function entry for the catalog function
    /// `name`/`arity` inside its virtual host document. When `discriminator`
    /// is present, the range identifies that exact overload. The document layout
    /// is deterministic (see [`Self::handle_host_document_content`]): each
    /// matching catalog entry renders one signature line, optionally followed
    /// by a `// description` line. The definition points at the signature
    /// line of the selected matching entry so a client that opens the virtual
    /// document lands exactly on the function.
    fn host_entry_range(
        &self,
        name: &str,
        arity: usize,
        discriminator: Option<&str>,
    ) -> serde_json::Value {
        let matches: Vec<&HostFunctionSchema> = self
            .catalog
            .functions()
            .iter()
            .filter(|f| f.name == name)
            .filter(|f| f.params.len() == arity)
            .collect();
        let selected = match discriminator {
            Some(discriminator) => matches
                .iter()
                .position(|function| function.identity_discriminator() == discriminator),
            None => (!matches.is_empty()).then_some(0),
        };
        let (line, length) = if let Some(selected) = selected {
            let schema = matches[selected];
            let line = matches[..selected]
                .iter()
                .map(|function| 1 + usize::from(!function.description.is_empty()))
                .sum();
            let signature = render_host_signature(schema);
            (line, signature.chars().count())
        } else {
            // Unknown function: the virtual document renders a comment line.
            (
                0,
                format!("// Unknown host function: {name} (arity {arity})")
                    .chars()
                    .count(),
            )
        };
        serde_json::json!({
            "start": { "line": line, "character": 0 },
            "end": { "line": line, "character": length },
        })
    }
}

/// Parse a host definition label (`host://<name>/<arity>[/<identity>] — <desc>`)
/// into its canonical name, arity and optional exact overload discriminator.
fn parse_host_label(label: &str) -> (String, usize, Option<String>) {
    let rest = label
        .strip_prefix("host://")
        .or_else(|| label.strip_prefix(&format!("{HOST_SCHEME}://")))
        .unwrap_or(label);
    let rest = rest.split(" — ").next().unwrap_or(rest);
    split_host_uri(rest)
}

fn split_host_uri(rest: &str) -> (String, usize, Option<String>) {
    let mut parts = rest.rsplitn(3, '/');
    let last = parts.next().unwrap_or_default();
    let previous = parts.next();
    let name = parts.next();
    match (name, previous) {
        (Some(name), Some(arity)) => (
            name.to_string(),
            arity.parse().unwrap_or(0),
            Some(last.to_string()),
        ),
        (None, Some(name)) => (name.to_string(), last.parse().unwrap_or(0), None),
        _ => (rest.to_string(), 0, None),
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut catalog_path: Option<PathBuf> = None;
    let mut config = ServerConfig::default();
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
            "--max-message-bytes" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("rustscript-lsp: --max-message-bytes requires a byte count");
                    std::process::exit(2);
                }
                match args[i].parse::<usize>() {
                    Ok(value) if value > 0 => config.max_message_bytes = value,
                    _ => {
                        eprintln!("rustscript-lsp: --max-message-bytes must be a positive integer");
                        std::process::exit(2);
                    }
                }
            }
            "--max-document-chars" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("rustscript-lsp: --max-document-chars requires a char count");
                    std::process::exit(2);
                }
                match args[i].parse::<usize>() {
                    Ok(value) if value > 0 => config.max_document_chars = value,
                    _ => {
                        eprintln!(
                            "rustscript-lsp: --max-document-chars must be a positive integer"
                        );
                        std::process::exit(2);
                    }
                }
            }
            "--help" | "-h" => {
                println!(
                    "rustscript-lsp — resource-aware RustScript language server (LSP over stdio)\n\n\
                     USAGE:\n    rustscript-lsp [OPTIONS]\n\n\
                     Reads JSON-RPC messages from stdin, writes responses to stdout.\n\
                     OPTIONS:\n\
                     \x20 --catalog <host-api-catalog.json>   custom HostApiCatalog snapshot (same serde\n\
                     \x20                                   shape the compiler validates); defaults to the\n\
                     \x20                                   standard sqlite+io+http catalog.\n\
                     \x20 --max-message-bytes <n>            per-message payload cap (default 16 MiB).\n\
                     \x20 --max-document-chars <n>           per-document text cap (default 8 Mi chars).\n"
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

    let mut server = LspServer::new(catalog, config);
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        let msg = match read_message(&mut reader, config.max_message_bytes) {
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
        if let Some(response) = response
            && let Err(err) = write_message(&mut out, &response)
        {
            eprintln!("rustscript-lsp: failed writing response: {err}");
            std::process::exit(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding #5: `uri_for_source_name` must never double the host scheme and
    /// must map both legacy `host://` and canonical `rustscript-host://`
    /// names to the canonical scheme, and open documents to their URIs.
    #[test]
    fn uri_for_source_name_maps_host_schemes_without_double_prefix() {
        let config = ServerConfig::default();
        let server = LspServer::new(standard_catalog(), config);

        // Canonical host-scheme names pass through untouched.
        assert_eq!(
            server.uri_for_source_name("rustscript-host://sqlite::open/1"),
            "rustscript-host://sqlite::open/1"
        );
        // Legacy `host://` names are upgraded to the canonical scheme once.
        assert_eq!(
            server.uri_for_source_name("host://sqlite::query/4"),
            "rustscript-host://sqlite::query/4"
        );
        // Plain file names map to canonical file URIs.
        assert_eq!(
            server.uri_for_source_name("/tmp/foo.rss"),
            "file:///tmp/foo.rss"
        );
    }

    #[test]
    fn uri_for_source_name_prefers_open_document_uri() {
        let config = ServerConfig::default();
        let mut server = LspServer::new(standard_catalog(), config);
        // The document's canonical identity (a nonexistent buffer keeps its
        // normalized absolute path).
        let identity = canonical_identity(Path::new("/tmp/fixture/main.rss"));
        let identity_str = normalized_source_name(&identity.to_string_lossy());
        server.documents.insert(
            "file:///tmp/fixture/main.rss".to_string(),
            Document::new(
                "file:///tmp/fixture/main.rss".to_string(),
                identity,
                "fn main() {}\n".to_string(),
            ),
        );
        // The SourceMap name (canonical identity) maps to the open document's URI.
        assert_eq!(
            server.uri_for_source_name(&identity_str),
            "file:///tmp/fixture/main.rss"
        );
        // A source name recorded with the same canonical identity in any
        // slash-normalized spelling maps identically.
        assert_eq!(
            server.uri_for_source_name(&normalized_source_name(&identity_str)),
            "file:///tmp/fixture/main.rss"
        );
    }

    #[test]
    fn canonical_identity_matches_loader_semantics() {
        // A nonexistent absolute path keeps its normalized absolute form.
        assert_eq!(
            canonical_identity(Path::new("/no/such/dir/../virtual/nested.rss")),
            PathBuf::from("/no/such/virtual/nested.rss")
        );
        // A relative nonexistent path is anchored to the current directory.
        let anchored = canonical_identity(Path::new("virtual/nested.rss"));
        assert!(anchored.is_absolute(), "identity must be absolute");
        assert!(anchored.ends_with("virtual/nested.rss"));
    }

    #[test]
    fn duplicate_initialize_is_rejected_in_handle() {
        let config = ServerConfig::default();
        let mut server = LspServer::new(standard_catalog(), config);
        let msg = RpcMessage {
            id: Some(serde_json::json!(1)),
            method: "initialize".to_string(),
            params: serde_json::json!({}),
        };
        let mut out = Vec::new();
        let response = server
            .handle(&msg, &mut out)
            .expect("handle must not fail")
            .expect("initialize must respond");
        assert!(
            response.get("result").is_some(),
            "first initialize succeeds"
        );
        // Second initialize: rejected with InvalidRequest.
        let response = server
            .handle(&msg, &mut out)
            .expect("handle must not fail")
            .expect("second initialize must respond");
        let error = response.get("error").expect("error object");
        assert_eq!(
            error["code"],
            serde_json::json!(-32600),
            "duplicate initialize must return InvalidRequest"
        );
    }

    #[test]
    fn read_message_bounds_header_line_length() {
        // A single header line far beyond the cap must be a fatal framing
        // error, never an unbounded allocation.
        let bytes = format!(
            "X-Padding: {}\r\n\r\n{{}}\r\n",
            "a".repeat(MAX_HEADER_LINE_BYTES + 64)
        );
        let mut reader = std::io::BufReader::new(bytes.as_bytes());
        let outcome = read_message(&mut reader, MAX_MESSAGE_BYTES);
        match outcome {
            ReadOutcome::Fatal(message) => {
                assert!(
                    message.contains("size cap"),
                    "oversized header must be fatal: {message}"
                );
            }
            other => panic!("oversized header line must be fatal, got {other:?}"),
        }
    }

    #[test]
    fn read_message_bounds_header_total_bytes() {
        // Many small header lines whose cumulative size exceeds the cap.
        let mut bytes = String::new();
        for i in 0..(MAX_HEADER_TOTAL_BYTES / 64 + 2) {
            bytes.push_str(&format!("X-H{}-H: {}\r\n", i, "b".repeat(60)));
        }
        bytes.push_str("\r\n{}\r\n");
        let mut reader = std::io::BufReader::new(bytes.as_bytes());
        let outcome = read_message(&mut reader, MAX_MESSAGE_BYTES);
        match outcome {
            ReadOutcome::Fatal(message) => {
                assert!(
                    message.contains("size cap"),
                    "oversized header block must be fatal: {message}"
                );
            }
            other => panic!("oversized header block must be fatal, got {other:?}"),
        }
    }

    #[test]
    fn host_document_content_selects_the_exact_overload_identity() {
        use rustscript::host_api::{
            HostApiBuilder, HostParamSchema, ResourceTypeKey, ResourceTypeSchema,
        };

        let file_key = ResourceTypeKey::new("adapter.file").expect("valid file key");
        let database_key = ResourceTypeKey::new("adapter.database").expect("valid database key");
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(file_key.clone(), "an adapter file"));
        builder.resource(ResourceTypeSchema::new(
            database_key.clone(),
            "an adapter database",
        ));
        builder.function(
            HostFunctionSchema::with_return(
                "adapter::close",
                vec![HostParamSchema::with_passing(
                    "handle",
                    HostTypeSchema::Resource(file_key),
                    HostParamPassing::TakeOwned,
                )],
                HostTypeSchema::Bool,
            )
            .with_description("close the adapter file"),
        );
        builder.function(
            HostFunctionSchema::with_return(
                "adapter::close",
                vec![HostParamSchema::with_passing(
                    "connection",
                    HostTypeSchema::Resource(database_key),
                    HostParamPassing::TakeOwned,
                )],
                HostTypeSchema::Null,
            )
            .with_description("close the adapter database"),
        );
        let catalog = Arc::new(builder.build().expect("valid overload catalog"));
        let database = catalog.functions_named("adapter::close")[1];
        let uri = format!(
            "{HOST_SCHEME}://{}/{}/{}",
            database.name,
            database.params.len(),
            database.identity_discriminator()
        );
        let server = LspServer::new(catalog, ServerConfig::default());
        let content = server.handle_host_document_content(&serde_json::json!({ "uri": uri }));
        assert_eq!(
            content["content"],
            serde_json::json!(
                "adapter::close(connection: take_owned resource<adapter.database>) -> null\n// close the adapter database"
            )
        );
    }

    #[test]
    fn lsp_metadata_keeps_each_overload_for_signature_hover_completion_and_definition() {
        use rustscript::host_api::{
            HostApiBuilder, HostParamSchema, ResourceTypeKey, ResourceTypeSchema,
        };

        let file_key = ResourceTypeKey::new("adapter.file").expect("valid file key");
        let database_key = ResourceTypeKey::new("adapter.database").expect("valid database key");
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(file_key.clone(), "an adapter file"));
        builder.resource(ResourceTypeSchema::new(
            database_key.clone(),
            "an adapter database",
        ));
        builder.function(HostFunctionSchema::with_return(
            "adapter::make_file",
            Vec::new(),
            HostTypeSchema::Resource(file_key.clone()),
        ));
        builder.function(HostFunctionSchema::with_return(
            "adapter::make_database",
            Vec::new(),
            HostTypeSchema::Resource(database_key.clone()),
        ));
        builder.function(
            HostFunctionSchema::with_return(
                "adapter::close",
                vec![HostParamSchema::with_passing(
                    "handle",
                    HostTypeSchema::Resource(file_key),
                    HostParamPassing::TakeOwned,
                )],
                HostTypeSchema::Bool,
            )
            .with_description("close the adapter file"),
        );
        builder.function(
            HostFunctionSchema::with_return(
                "adapter::close",
                vec![HostParamSchema::with_passing(
                    "connection",
                    HostTypeSchema::Resource(database_key),
                    HostParamPassing::TakeOwned,
                )],
                HostTypeSchema::Null,
            )
            .with_description("close the adapter database"),
        );
        let mut server = LspServer::new(
            Arc::new(builder.build().expect("valid overload catalog")),
            ServerConfig::default(),
        );
        let uri = "file:///tmp/rustscript-lsp-overloads/main.rss";
        let source = "use adapter;\nlet file = adapter::make_file();\nlet db = adapter::make_database();\nadapter::close(file);\nadapter::close(db);\n";
        let mut out = Vec::new();
        server
            .handle_did_open(
                &serde_json::json!({
                    "textDocument": { "uri": uri, "text": source }
                }),
                &mut out,
            )
            .expect("overload document should open");

        let file_signature = server.handle_signature_help(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 2 }
        }));
        assert_eq!(
            file_signature["signatures"][0]["documentation"]["value"],
            serde_json::json!("close the adapter file")
        );
        assert!(
            file_signature["signatures"][0]["label"]
                .as_str()
                .unwrap_or("")
                .contains("resource<adapter.file>")
        );

        let database_signature = server.handle_signature_help(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 4, "character": 2 }
        }));
        assert_eq!(
            database_signature["signatures"][0]["documentation"]["value"],
            serde_json::json!("close the adapter database")
        );
        assert!(
            database_signature["signatures"][0]["label"]
                .as_str()
                .unwrap_or("")
                .contains("resource<adapter.database>")
        );

        let file_hover = server.handle_hover(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 2 }
        }));
        assert!(
            file_hover["contents"]["value"]
                .as_str()
                .unwrap_or("")
                .contains("bool")
        );
        let database_hover = server.handle_hover(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 4, "character": 2 }
        }));
        assert!(
            database_hover["contents"]["value"]
                .as_str()
                .unwrap_or("")
                .contains("null")
        );

        let completions = server.handle_completion(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 9 }
        }));
        let close_items: Vec<&serde_json::Value> = completions["items"]
            .as_array()
            .expect("completion items")
            .iter()
            .filter(|item| item["label"] == serde_json::json!("close"))
            .collect();
        assert_eq!(close_items.len(), 2, "both close overloads must complete");
        assert!(close_items.iter().any(|item| {
            item["documentation"] == serde_json::json!("close the adapter file")
                && item["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("resource<adapter.file>")
        }));
        assert!(close_items.iter().any(|item| {
            item["documentation"] == serde_json::json!("close the adapter database")
                && item["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("resource<adapter.database>")
        }));

        let file_definition = server.handle_definition(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 2 }
        }));
        let database_definition = server.handle_definition(&serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 4, "character": 2 }
        }));
        let file_uri = file_definition[0]["uri"].as_str().expect("file host uri");
        let database_uri = database_definition[0]["uri"]
            .as_str()
            .expect("database host uri");
        assert_ne!(
            file_uri, database_uri,
            "overloads need distinct definitions"
        );
        assert!(file_uri.contains("adapter::close/1/"));
        assert!(database_uri.contains("adapter::close/1/"));
    }

    struct AdversarialHeaderReader {
        bytes: Vec<u8>,
        offset: usize,
        chunk_size: usize,
        read_line_called: bool,
    }

    impl AdversarialHeaderReader {
        fn new(bytes: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                bytes,
                offset: 0,
                chunk_size: chunk_size.max(1),
                read_line_called: false,
            }
        }
    }

    impl std::io::Read for AdversarialHeaderReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let count = buffer
                .len()
                .min(self.chunk_size)
                .min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    impl std::io::BufRead for AdversarialHeaderReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = (self.offset + self.chunk_size).min(self.bytes.len());
            Ok(&self.bytes[self.offset..end])
        }

        fn consume(&mut self, amount: usize) {
            self.offset = (self.offset + amount).min(self.bytes.len());
        }

        fn read_line(&mut self, _buffer: &mut String) -> std::io::Result<usize> {
            self.read_line_called = true;
            Err(std::io::Error::other(
                "test reader read_line must not be called",
            ))
        }
    }

    fn framed_body(method: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": { "text": "é\r\n😀" },
        }))
        .expect("JSON body should serialize")
    }

    #[test]
    fn read_message_uses_bounded_incremental_headers_for_fragmented_utf8_payloads() {
        let body = framed_body("é");
        let mut frame = format!("Content-Length: {}\n\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        let mut reader = AdversarialHeaderReader::new(frame, 1);
        match read_message(&mut reader, MAX_MESSAGE_BYTES) {
            ReadOutcome::Message(message) => assert_eq!(message.method, "é"),
            other => panic!("fragmented UTF-8 frame should parse: {other:?}"),
        }
        assert!(!reader.read_line_called);
    }

    #[test]
    fn read_message_rejects_duplicate_content_length_deterministically() {
        let body = framed_body("duplicate");
        let mut frame = format!(
            "Content-Length: {}\r\nContent-Length: {}\r\n\r\n",
            body.len(),
            body.len()
        )
        .into_bytes();
        frame.extend_from_slice(&body);
        let mut reader = AdversarialHeaderReader::new(frame, 2);
        match read_message(&mut reader, MAX_MESSAGE_BYTES) {
            ReadOutcome::Fatal(message) => assert!(
                message.contains("duplicate Content-Length"),
                "duplicate header should have deterministic reason: {message}"
            ),
            other => panic!("duplicate Content-Length must be fatal: {other:?}"),
        }
    }

    #[test]
    fn read_message_rejects_invalid_content_length_deterministically() {
        let mut reader =
            AdversarialHeaderReader::new(b"Content-Length: not-a-number\r\n\r\n{}".to_vec(), 1);
        match read_message(&mut reader, MAX_MESSAGE_BYTES) {
            ReadOutcome::Fatal(message) => assert_eq!(message, "invalid Content-Length header"),
            other => panic!("invalid Content-Length must be fatal: {other:?}"),
        }
    }
    #[test]
    fn read_message_rejects_partial_header_at_eof() {
        let mut reader = AdversarialHeaderReader::new(b"Content-Length: 2".to_vec(), 1);
        match read_message(&mut reader, MAX_MESSAGE_BYTES) {
            ReadOutcome::Fatal(message) => assert!(
                message.contains("unexpected EOF"),
                "partial header should report truncation: {message}"
            ),
            other => panic!("partial header must be fatal: {other:?}"),
        }
    }

    #[test]
    fn read_message_rejects_invalid_utf8_header_without_consuming_unbounded_input() {
        let mut reader = AdversarialHeaderReader::new(b"X-Test: \xff\n\n".to_vec(), 1);
        match read_message(&mut reader, MAX_MESSAGE_BYTES) {
            ReadOutcome::Fatal(message) => assert!(
                message.contains("UTF-8"),
                "invalid UTF-8 header should be fatal: {message}"
            ),
            other => panic!("invalid UTF-8 header must be fatal: {other:?}"),
        }
        assert!(reader.offset <= MAX_HEADER_LINE_BYTES);
    }

    #[test]
    fn read_message_rejects_oversized_header_without_newline_before_consuming_it() {
        let bytes = vec![b'x'; MAX_HEADER_LINE_BYTES + 4096];
        let mut reader = AdversarialHeaderReader::new(bytes, MAX_HEADER_LINE_BYTES + 4096);
        match read_message(&mut reader, MAX_MESSAGE_BYTES) {
            ReadOutcome::Fatal(message) => assert!(
                message.contains("size cap"),
                "oversized header should be fatal: {message}"
            ),
            other => panic!("oversized header must be fatal: {other:?}"),
        }
        assert!(
            reader.offset <= MAX_HEADER_LINE_BYTES,
            "parser must reject before consuming an unbounded line"
        );
    }

    #[test]
    fn lsp_positions_reject_surrogate_pair_interior_and_out_of_range_columns() {
        let text = "a😀b";
        assert_eq!(lsp_position_to_offset(text, 0, 0), Some(0));
        assert_eq!(lsp_position_to_offset(text, 0, 1), Some(1));
        assert_eq!(
            lsp_position_to_offset(text, 0, 2),
            None,
            "the second UTF-16 code unit inside 😀 is not a byte boundary"
        );
        assert_eq!(lsp_position_to_offset(text, 0, 3), Some(5));
        assert_eq!(lsp_position_to_offset(text, 0, 4), Some(6));
        assert_eq!(lsp_position_to_offset(text, 0, 5), None);
        assert_eq!(offset_to_lsp_position(text, 1), Some((0, 1)));
        assert_eq!(offset_to_lsp_position(text, 5), Some((0, 3)));
        assert_eq!(offset_to_lsp_position(text, 2), None);
        assert_eq!(span_to_lsp(text, 1, 5), ((0, 1), (0, 3)));
        for (character, offset) in [(0, 0), (1, 1), (3, 5), (4, 6)] {
            assert_eq!(
                lsp_position_to_offset(text, 0, character),
                Some(offset),
                "valid UTF-16 boundary must round-trip"
            );
        }
        assert_eq!(offset_to_lsp_position(text, text.len() + 1), None);
    }

    #[test]
    fn lsp_positions_treat_crlf_as_line_end_and_keep_combining_scalars_addressable() {
        let text = "e\u{301}\r\nnext\n";
        assert_eq!(lsp_position_to_offset(text, 0, 0), Some(0));
        assert_eq!(lsp_position_to_offset(text, 0, 1), Some(1));
        assert_eq!(lsp_position_to_offset(text, 0, 2), Some(3));
        assert_eq!(lsp_position_to_offset(text, 0, 3), None);
        assert_eq!(lsp_position_to_offset(text, 1, 0), Some(5));
        assert_eq!(lsp_position_to_offset(text, 1, 4), Some(9));
        assert_eq!(lsp_position_to_offset(text, 1, 5), None);
        assert_eq!(lsp_position_to_offset(text, 2, 0), Some(text.len()));
        assert_eq!(lsp_position_to_offset(text, 3, 0), None);
        assert_eq!(offset_to_lsp_position(text, 3), Some((0, 2)));
        assert_eq!(offset_to_lsp_position(text, 4), Some((0, 2)));
        assert_eq!(offset_to_lsp_position(text, 5), Some((1, 0)));
        assert_eq!(offset_to_lsp_position(text, text.len()), Some((2, 0)));
    }

    fn open_virtual_document(server: &mut LspServer, uri: &str, text: &str) {
        let mut out = Vec::new();
        server
            .handle_did_open(
                &serde_json::json!({
                    "textDocument": { "uri": uri, "text": text }
                }),
                &mut out,
            )
            .expect("virtual document should open");
    }

    #[test]
    fn opening_a_missing_virtual_dependency_refreshes_an_unknown_importer() {
        let mut server = LspServer::new(standard_catalog(), ServerConfig::default());
        let a_uri = "file:///tmp/rustscript-lsp-missing-graph/a.rss";
        let b_uri = "file:///tmp/rustscript-lsp-missing-graph/b.rss";
        open_virtual_document(
            &mut server,
            a_uri,
            "use self::b;\nlet value = b::value();\n",
        );
        assert!(
            server
                .documents
                .get(a_uri)
                .is_some_and(|doc| doc.model.is_none()),
            "an importer with a missing dependency starts without a model"
        );
        open_virtual_document(&mut server, b_uri, "pub fn value() -> int { 1 }\n");
        assert!(
            server
                .documents
                .get(a_uri)
                .is_some_and(|doc| doc.model.is_some()),
            "opening the missing dependency must refresh the importer"
        );
    }

    #[test]
    fn cyclic_virtual_dependencies_are_invalidated_without_recursion() {
        let mut server = LspServer::new(standard_catalog(), ServerConfig::default());
        let a_uri = "file:///tmp/rustscript-lsp-cycle-graph/a.rss";
        let b_uri = "file:///tmp/rustscript-lsp-cycle-graph/b.rss";
        open_virtual_document(
            &mut server,
            a_uri,
            "use self::b;\nfn main() { b::value(); }\n",
        );
        open_virtual_document(
            &mut server,
            b_uri,
            "use self::a;\npub fn value() { a::main(); }\n",
        );
        let mut out = Vec::new();
        server
            .handle_did_change(
                &serde_json::json!({
                    "textDocument": { "uri": a_uri },
                    "contentChanges": [{ "text": "use self::b;\nfn main() { b::value(); }\n" }]
                }),
                &mut out,
            )
            .expect("cyclic dependency change should terminate");
        assert!(server.documents.contains_key(a_uri));
        assert!(server.documents.contains_key(b_uri));
    }
    #[test]
    fn changing_a_transitive_virtual_dependency_refreshes_open_importers() {
        let mut server = LspServer::new(standard_catalog(), ServerConfig::default());
        let c_uri = "file:///tmp/rustscript-lsp-graph/c.rss";
        let b_uri = "file:///tmp/rustscript-lsp-graph/b.rss";
        let a_uri = "file:///tmp/rustscript-lsp-graph/a.rss";
        open_virtual_document(&mut server, c_uri, "pub fn value() -> int { 1 }\n");
        open_virtual_document(
            &mut server,
            b_uri,
            "use self::c;\npub fn bridge() { c::value() }\n",
        );
        open_virtual_document(
            &mut server,
            a_uri,
            "use self::b;\nlet result = b::bridge();\n",
        );
        let c_identity = canonical_identity(Path::new("/tmp/rustscript-lsp-graph/c.rss"));
        let old_source = server
            .documents
            .get(a_uri)
            .and_then(|doc| doc.model.as_ref())
            .and_then(|model| {
                let id = model
                    .sources()
                    .source_id_by_name(&normalized_source_name(&c_identity.to_string_lossy()))?;
                model.sources().source(id)
            });
        assert_eq!(old_source, Some("pub fn value() -> int { 1 }\n"));
        assert!(
            server
                .documents
                .get(a_uri)
                .map(|doc| doc.dependencies.contains(&c_identity))
                .unwrap_or(false),
            "the root document must retain the transitive canonical dependency"
        );
        assert!(
            server
                .dependents
                .get(&c_identity)
                .map(|importers| importers.contains(a_uri) && importers.contains(b_uri))
                .unwrap_or(false),
            "reverse dependency index must include both open importers"
        );
        let mut out = Vec::new();
        server
            .handle_did_change(
                &serde_json::json!({
                    "textDocument": { "uri": c_uri },
                    "contentChanges": [{ "text": "pub fn value() -> string { \"changed\" }\n" }]
                }),
                &mut out,
            )
            .expect("dependency change should be handled");
        let new_source = server
            .documents
            .get(a_uri)
            .and_then(|doc| doc.model.as_ref())
            .and_then(|model| {
                let id = model
                    .sources()
                    .source_id_by_name(&normalized_source_name(&c_identity.to_string_lossy()))?;
                model.sources().source(id)
            });
        assert_eq!(
            new_source,
            Some("pub fn value() -> string { \"changed\" }\n")
        );
    }

    #[test]
    fn closing_a_virtual_dependency_invalidates_transitive_importers() {
        let mut server = LspServer::new(standard_catalog(), ServerConfig::default());
        let c_uri = "file:///tmp/rustscript-lsp-close-graph/c.rss";
        let b_uri = "file:///tmp/rustscript-lsp-close-graph/b.rss";
        let a_uri = "file:///tmp/rustscript-lsp-close-graph/a.rss";
        open_virtual_document(&mut server, c_uri, "pub fn value() -> int { 1 }\n");
        open_virtual_document(
            &mut server,
            b_uri,
            "use self::c;\npub fn bridge() { c::value() }\n",
        );
        open_virtual_document(
            &mut server,
            a_uri,
            "use self::b;\nfn main() { b::bridge(); }\n",
        );
        assert!(
            server
                .documents
                .get(a_uri)
                .and_then(|doc| doc.model.as_ref())
                .is_some()
        );
        assert!(
            server
                .documents
                .get(b_uri)
                .and_then(|doc| doc.model.as_ref())
                .is_some()
        );

        let mut out = Vec::new();
        server
            .handle_did_close(
                &serde_json::json!({ "textDocument": { "uri": c_uri } }),
                &mut out,
            )
            .expect("dependency close should be handled");
        assert!(
            server
                .documents
                .get(a_uri)
                .and_then(|doc| doc.model.as_ref())
                .is_none(),
            "the root importer must not retain a model after its dependency closes"
        );
        assert!(
            server
                .documents
                .get(b_uri)
                .and_then(|doc| doc.model.as_ref())
                .is_none(),
            "the direct importer must not retain a model after its dependency closes"
        );
    }
}
