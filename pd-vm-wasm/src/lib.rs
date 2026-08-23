mod analyzer;
mod completions;
#[cfg(feature = "runtime")]
mod runtime;
mod stdlib;

use std::path::Path;

use serde::{Deserialize, Serialize};
use vm::{
    CompileSourceFileOptions, InferredLocalTypeHint, SourceFlavor,
    collect_inferred_local_type_hints_at_path_with_options,
    collect_inferred_local_type_hints_with_options, format_source_with_flavor,
};

use crate::analyzer::{
    LintDiagnostic, LintReport, LintSpan, lint_source_with_flavor, lint_source_with_flavor_at_path,
};
use crate::completions::{CompletionCatalog, build_completion_catalog};
#[cfg(feature = "runtime")]
use crate::runtime::{
    DebugCommand, DebugReport, FuelConfig, FuelState, InterruptModeState, RunCommand,
    RunErrorDetails, RunReport, debug_state, run_command, run_debug_command,
    start_debug_source_with_flavor, start_run_source_with_flavor,
};

#[derive(Serialize)]
struct LintResponse {
    diagnostics: Vec<LintDiagnosticJson>,
}

#[derive(Serialize)]
struct LocalTypeHintsResponse {
    hints: Vec<LocalTypeHintJson>,
}

#[derive(Serialize)]
struct FormatResponse {
    ok: bool,
    formatted: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct LintDiagnosticJson {
    line: usize,
    severity: &'static str,
    message: String,
    span: Option<LintSpanJson>,
    rendered: String,
}

#[derive(Serialize)]
struct LintSpanJson {
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
}

#[derive(Serialize)]
struct LocalTypeHintJson {
    name: String,
    inferred_type: String,
    declared_line: Option<u32>,
    last_line: Option<u32>,
}

#[cfg(feature = "runtime")]
#[derive(Serialize)]
struct RunResponse {
    ok: bool,
    diagnostics: Vec<LintDiagnosticJson>,
    output: Vec<String>,
    stack: Vec<String>,
    error: Option<String>,
    error_code: Option<String>,
    error_details: Option<RunErrorDetails>,
    halted: bool,
    yielded: bool,
    command_output: String,
    fuel: FuelStateJson,
}

#[cfg(feature = "runtime")]
#[derive(Serialize)]
struct DebugResponse {
    diagnostics: Vec<LintDiagnosticJson>,
    output: Vec<String>,
    stack: Vec<String>,
    error: Option<String>,
    current_line: Option<u32>,
    breakpoints: Vec<u32>,
    halted: bool,
    command_output: String,
    fuel: FuelStateJson,
}

#[cfg(feature = "runtime")]
#[derive(Serialize)]
struct FuelStateJson {
    enabled: bool,
    mode: &'static str,
    remaining: Option<u64>,
    check_interval: u32,
    epoch_current: u64,
    epoch_deadline: Option<u64>,
    epoch_slice: Option<u64>,
}

#[derive(Deserialize)]
struct ModuleOverrideInput {
    path: String,
    source: String,
}

fn parse_flavor(raw: &str) -> SourceFlavor {
    match raw.trim().to_ascii_lowercase().as_str() {
        "javascript" | "js" => SourceFlavor::JavaScript,
        "lua" => SourceFlavor::Lua,
        _ => SourceFlavor::RustScript,
    }
}

fn pack_ptr_len(ptr: *mut u8, len: usize) -> u64 {
    ((len as u64) << 32) | (ptr as u64)
}

fn unpack_input<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if ptr == 0 || len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

fn leak_bytes(bytes: Vec<u8>) -> u64 {
    let owned = bytes.into_boxed_slice();
    let len = owned.len();
    let ptr = Box::into_raw(owned) as *mut u8;
    pack_ptr_len(ptr, len)
}

fn lint_diagnostic_to_json(item: LintDiagnostic) -> LintDiagnosticJson {
    LintDiagnosticJson {
        line: item.line,
        severity: item.severity.as_str(),
        message: item.message,
        span: item.span.map(lint_span_to_json),
        rendered: item.rendered,
    }
}

fn lint_span_to_json(span: LintSpan) -> LintSpanJson {
    LintSpanJson {
        start_line: span.start_line,
        start_col: span.start_col,
        end_line: span.end_line,
        end_col: span.end_col,
    }
}

fn lint_response_to_json(report: LintReport) -> Vec<u8> {
    let response = LintResponse {
        diagnostics: report
            .diagnostics
            .into_iter()
            .map(lint_diagnostic_to_json)
            .collect(),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec())
}

fn local_type_hints_to_json(hints: Vec<InferredLocalTypeHint>) -> Vec<u8> {
    let response = LocalTypeHintsResponse {
        hints: hints.into_iter().map(local_type_hint_to_json).collect(),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"hints\":[]}".to_vec())
}

fn format_response_to_json(result: Result<String, vm::FormatError>) -> Vec<u8> {
    let response = match result {
        Ok(formatted) => FormatResponse {
            ok: true,
            formatted: Some(formatted),
            error: None,
        },
        Err(err) => FormatResponse {
            ok: false,
            formatted: None,
            error: Some(err.to_string()),
        },
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"formatted\":null,\"error\":\"format failed\"}".to_vec()
    })
}

fn local_type_hints_with_flavor(source: &str, flavor: SourceFlavor) -> Vec<InferredLocalTypeHint> {
    collect_inferred_local_type_hints_with_options(
        source,
        flavor,
        stdlib::embedded_stdlib_compile_options(),
    )
    .unwrap_or_default()
}

fn local_type_hints_with_flavor_at_path(
    source: &str,
    path: &Path,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Vec<InferredLocalTypeHint> {
    collect_inferred_local_type_hints_at_path_with_options(path, source, flavor, options)
        .unwrap_or_default()
}

#[cfg(feature = "runtime")]
fn lint_diagnostic_json_to_value(diagnostic: &LintDiagnosticJson) -> serde_json::Value {
    serde_json::json!({
        "line": diagnostic.line,
        "severity": diagnostic.severity,
        "message": diagnostic.message,
        "span": diagnostic.span.as_ref().map(|span| serde_json::json!({
            "start_line": span.start_line,
            "start_col": span.start_col,
            "end_line": span.end_line,
            "end_col": span.end_col,
        })),
        "rendered": diagnostic.rendered,
    })
}

/// Total serialization fallback dedicated to [`RunResponse`].
///
/// The normal path serialises the response struct; if that ever fails (e.g.
/// under allocation pressure) this builds the same JSON payload directly from
/// the primitive fields via `serde_json::json!`, bypassing the failing struct
/// serialiser entirely. It never drops the structured error surface, so a JS
/// consumer can still match `error`, stable `error_code`, and structured
/// `error_details` even on the fallback path. No sub-serialisation is routed
/// back through [`RunResponse`], so this cannot fail recursively and can never
/// yield `null` for the error fields that were present in the response.
#[cfg(feature = "runtime")]
fn run_response_fallback(response: &RunResponse) -> Vec<u8> {
    let error_details = response.error_details.as_ref().map(|details| {
        serde_json::json!({
            "operation": details.operation,
            "message": details.message,
            "limit": details.limit,
            "value": details.value,
        })
    });
    let diagnostics = response
        .diagnostics
        .iter()
        .map(lint_diagnostic_json_to_value)
        .collect::<Vec<_>>();
    let fuel = serde_json::json!({
        "enabled": response.fuel.enabled,
        "mode": response.fuel.mode,
        "remaining": response.fuel.remaining,
        "check_interval": response.fuel.check_interval,
        "epoch_current": response.fuel.epoch_current,
        "epoch_deadline": response.fuel.epoch_deadline,
        "epoch_slice": response.fuel.epoch_slice,
    });
    let payload = serde_json::json!({
        "ok": response.ok,
        "diagnostics": diagnostics,
        "output": response.output,
        "stack": response.stack,
        "error": response.error,
        "error_code": response.error_code,
        "error_details": error_details,
        "halted": response.halted,
        "yielded": response.yielded,
        "command_output": response.command_output,
        "fuel": fuel,
    });
    serde_json::to_vec(&payload).unwrap_or_else(|_| {
        // The payload is constructed exclusively from plain JSON data (strings,
        // numbers, booleans, optionals), so serialization cannot fail; this
        // arm exists only to satisfy the fallible API and must never surface
        // null error fields. Keep the error surface alive regardless.
        build_error_only_fallback(response)
    })
}

/// Last-resort error-only payload for the pathological case where even the
/// plain-data fallback serialization fails. Preserves the compatibility
/// message, stable code, and structured details via manual JSON escaping so
/// the structured error surface can never be dropped.
#[cfg(feature = "runtime")]
fn build_error_only_fallback(response: &RunResponse) -> Vec<u8> {
    fn escape_json_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    let error = response.error.as_deref().unwrap_or("");
    let error_code = response.error_code.as_deref().unwrap_or("vm_error");
    let details = response.error_details.as_ref();
    let operation = details.map(|d| d.operation.as_str()).unwrap_or("vm");
    let message = details.map(|d| d.message.as_str()).unwrap_or(error);
    let limit = details.and_then(|d| d.limit);
    let value = details.and_then(|d| d.value);

    let mut body = String::new();
    body.push_str("{\"ok\":false,\"error\":");
    body.push_str(&escape_json_string(error));
    body.push_str(",\"error_code\":");
    body.push_str(&escape_json_string(error_code));
    body.push_str(",\"error_details\":{\"operation\":");
    body.push_str(&escape_json_string(operation));
    body.push_str(",\"message\":");
    body.push_str(&escape_json_string(message));
    body.push_str(",\"limit\":");
    body.push_str(
        &limit
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string()),
    );
    body.push_str(",\"value\":");
    body.push_str(
        &value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string()),
    );
    body.push_str("}}");
    body.into_bytes()
}

/// Serialise a [`RunResponse`] for the JS boundary, falling back to a total,
/// structured-error-preserving JSON builder if struct serialisation fails.
#[cfg(feature = "runtime")]
fn serialize_run_response(response: &RunResponse) -> Vec<u8> {
    serde_json::to_vec(response).unwrap_or_else(|_| run_response_fallback(response))
}

#[cfg(feature = "runtime")]
fn run_response_to_json(report: RunReport) -> Vec<u8> {
    let ok = report.error.is_none();
    let response = RunResponse {
        ok,
        diagnostics: report
            .diagnostics
            .into_iter()
            .map(lint_diagnostic_to_json)
            .collect(),
        output: report.output,
        stack: report.stack,
        error: report.error,
        error_code: report.error_code,
        error_details: report.error_details,
        halted: report.halted,
        yielded: report.yielded,
        command_output: report.command_output,
        fuel: fuel_state_to_json(report.fuel),
    };
    serialize_run_response(&response)
}

#[cfg(feature = "runtime")]
fn debug_response_to_json(report: DebugReport) -> Vec<u8> {
    let response = DebugResponse {
        diagnostics: report
            .diagnostics
            .into_iter()
            .map(lint_diagnostic_to_json)
            .collect(),
        output: report.output,
        stack: report.stack,
        error: report.error,
        current_line: report.current_line,
        breakpoints: report.breakpoints,
        halted: report.halted,
        command_output: report.command_output,
        fuel: fuel_state_to_json(report.fuel),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"diagnostics\":[],\"output\":[],\"stack\":[],\"breakpoints\":[],\"halted\":true,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

#[cfg(feature = "runtime")]
fn fuel_state_to_json(fuel: FuelState) -> FuelStateJson {
    FuelStateJson {
        enabled: fuel.enabled,
        mode: match fuel.mode {
            InterruptModeState::None => "none",
            InterruptModeState::Fuel => "fuel",
            InterruptModeState::Epoch => "epoch",
        },
        remaining: fuel.remaining,
        check_interval: fuel.check_interval,
        epoch_current: fuel.epoch_current,
        epoch_deadline: fuel.epoch_deadline,
        epoch_slice: fuel.epoch_slice,
    }
}

fn completion_catalog_to_json(catalog: CompletionCatalog) -> Vec<u8> {
    serde_json::to_vec(&catalog)
        .unwrap_or_else(|_| b"{\"rustscript\":[],\"javascript\":[],\"lua\":[]}".to_vec())
}

fn local_type_hint_to_json(hint: InferredLocalTypeHint) -> LocalTypeHintJson {
    LocalTypeHintJson {
        name: hint.name,
        inferred_type: hint.inferred_type,
        declared_line: hint.declared_line,
        last_line: hint.last_line,
    }
}

fn parse_module_overrides(raw: &str) -> CompileSourceFileOptions {
    let mut options = stdlib::embedded_stdlib_compile_options();
    let parsed = serde_json::from_str::<Vec<ModuleOverrideInput>>(raw).unwrap_or_default();
    for entry in parsed {
        if entry.path.trim().is_empty() {
            continue;
        }
        options.set_module_override_source(entry.path, entry.source);
    }
    options
}

fn invalid_utf8_lint_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = LintResponse {
        diagnostics: vec![LintDiagnosticJson {
            line: 1,
            severity: "error",
            message: format!("invalid utf-8 {label}: {err}"),
            span: None,
            rendered: format!("invalid utf-8 {label}: {err}"),
        }],
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec())
}

fn invalid_utf8_local_type_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = LocalTypeHintsResponse { hints: Vec::new() };
    let _ = (label, err);
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"hints\":[]}".to_vec())
}

fn invalid_utf8_format_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = FormatResponse {
        ok: false,
        formatted: None,
        error: Some(format!("invalid utf-8 {label}: {err}")),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"formatted\":null,\"error\":\"invalid utf-8\"}".to_vec()
    })
}

#[cfg(feature = "runtime")]
fn invalid_utf8_run_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = RunResponse {
        ok: false,
        diagnostics: vec![LintDiagnosticJson {
            line: 1,
            severity: "error",
            message: format!("invalid utf-8 {label}: {err}"),
            span: None,
            rendered: format!("invalid utf-8 {label}: {err}"),
        }],
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!("invalid utf-8 {label}: {err}")),
        error_code: Some("input_error".to_string()),
        error_details: Some(RunErrorDetails {
            operation: "wasm::input".to_string(),
            message: format!("invalid utf-8 {label}: {err}"),
            limit: None,
            value: None,
        }),
        halted: true,
        yielded: false,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }),
    };
    serialize_run_response(&response)
}

#[cfg(feature = "runtime")]
fn invalid_utf8_debug_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = DebugResponse {
        diagnostics: vec![LintDiagnosticJson {
            line: 1,
            severity: "error",
            message: format!("invalid utf-8 {label}: {err}"),
            span: None,
            rendered: format!("invalid utf-8 {label}: {err}"),
        }],
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!("invalid utf-8 {label}: {err}")),
        current_line: None,
        breakpoints: Vec::new(),
        halted: true,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"diagnostics\":[],\"output\":[],\"stack\":[],\"breakpoints\":[],\"halted\":true,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

#[cfg(feature = "runtime")]
fn invalid_run_command_response(command_json: &str, error: &str) -> Vec<u8> {
    let response = RunResponse {
        ok: false,
        diagnostics: Vec::new(),
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!(
            "invalid run command: {error}; payload={command_json}"
        )),
        error_code: Some("input_error".to_string()),
        error_details: Some(RunErrorDetails {
            operation: "wasm::run_command".to_string(),
            message: error.to_string(),
            limit: None,
            value: None,
        }),
        halted: true,
        yielded: false,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }),
    };
    serialize_run_response(&response)
}

#[cfg(feature = "runtime")]
fn invalid_debug_command_response(command_json: &str, error: &str) -> Vec<u8> {
    let response = DebugResponse {
        diagnostics: Vec::new(),
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!(
            "invalid debug command: {error}; payload={command_json}"
        )),
        current_line: None,
        breakpoints: Vec::new(),
        halted: true,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"diagnostics\":[],\"output\":[],\"stack\":[],\"breakpoints\":[],\"halted\":true,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

#[cfg(feature = "runtime")]
fn invalid_run_options_response(options_json: &str, error: &str) -> Vec<u8> {
    let response = RunResponse {
        ok: false,
        diagnostics: Vec::new(),
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!(
            "invalid run options: {error}; payload={options_json}"
        )),
        error_code: Some("input_error".to_string()),
        error_details: Some(RunErrorDetails {
            operation: "wasm::run_options".to_string(),
            message: error.to_string(),
            limit: None,
            value: None,
        }),
        halted: true,
        yielded: false,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }),
    };
    serialize_run_response(&response)
}

#[cfg(feature = "runtime")]
fn invalid_debug_options_response(options_json: &str, error: &str) -> Vec<u8> {
    let response = DebugResponse {
        diagnostics: Vec::new(),
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!(
            "invalid debug options: {error}; payload={options_json}"
        )),
        current_line: None,
        breakpoints: Vec::new(),
        halted: true,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"diagnostics\":[],\"output\":[],\"stack\":[],\"breakpoints\":[],\"halted\":true,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_dealloc(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 {
        return;
    }
    unsafe {
        let _ = Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lint_source_json(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_lint_response("source", &err)),
    };
    let flavor_raw = match std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_lint_response("flavor", &err)),
    };
    let report = lint_source_with_flavor(source, parse_flavor(flavor_raw));
    leak_bytes(lint_response_to_json(report))
}

#[unsafe(no_mangle)]
pub extern "C" fn format_source_json(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_format_response("source", &err)),
    };
    let flavor_raw = match std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_format_response("flavor", &err)),
    };
    leak_bytes(format_response_to_json(format_source_with_flavor(
        source,
        parse_flavor(flavor_raw),
    )))
}

#[unsafe(no_mangle)]
pub extern "C" fn lint_source_json_with_context(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
    path_ptr: u32,
    path_len: u32,
    overrides_ptr: u32,
    overrides_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_lint_response("source", &err)),
    };
    let flavor_raw = std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)).unwrap_or("rss");
    let path_raw = std::str::from_utf8(unpack_input(path_ptr, path_len)).unwrap_or("");
    let overrides_raw =
        std::str::from_utf8(unpack_input(overrides_ptr, overrides_len)).unwrap_or("[]");
    let options = parse_module_overrides(overrides_raw);

    let report = if path_raw.trim().is_empty() {
        lint_source_with_flavor(source, parse_flavor(flavor_raw))
    } else {
        lint_source_with_flavor_at_path(
            source,
            Path::new(path_raw),
            parse_flavor(flavor_raw),
            options,
        )
    };
    leak_bytes(lint_response_to_json(report))
}

#[unsafe(no_mangle)]
pub extern "C" fn local_type_hints_json(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_local_type_response("source", &err)),
    };
    let flavor_raw = match std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_local_type_response("flavor", &err)),
    };
    let hints = local_type_hints_with_flavor(source, parse_flavor(flavor_raw));
    leak_bytes(local_type_hints_to_json(hints))
}

#[unsafe(no_mangle)]
pub extern "C" fn local_type_hints_json_with_context(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
    path_ptr: u32,
    path_len: u32,
    overrides_ptr: u32,
    overrides_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_local_type_response("source", &err)),
    };
    let flavor_raw = std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)).unwrap_or("rss");
    let path_raw = std::str::from_utf8(unpack_input(path_ptr, path_len)).unwrap_or("");
    let overrides_raw =
        std::str::from_utf8(unpack_input(overrides_ptr, overrides_len)).unwrap_or("[]");
    let options = parse_module_overrides(overrides_raw);

    let hints = if path_raw.trim().is_empty() {
        collect_inferred_local_type_hints_with_options(source, parse_flavor(flavor_raw), options)
            .unwrap_or_default()
    } else {
        local_type_hints_with_flavor_at_path(
            source,
            Path::new(path_raw),
            parse_flavor(flavor_raw),
            options,
        )
    };

    leak_bytes(local_type_hints_to_json(hints))
}

#[cfg(feature = "runtime")]
#[unsafe(no_mangle)]
pub extern "C" fn run_source_json(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
    options_ptr: u32,
    options_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_run_response("source", &err)),
    };
    let flavor_raw = match std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_run_response("flavor", &err)),
    };
    let fuel_config = if options_len == 0 {
        FuelConfig::default()
    } else {
        let options_json = match std::str::from_utf8(unpack_input(options_ptr, options_len)) {
            Ok(value) => value,
            Err(err) => return leak_bytes(invalid_utf8_run_response("options", &err)),
        };
        match serde_json::from_str::<FuelConfig>(options_json) {
            Ok(value) => value,
            Err(err) => {
                return leak_bytes(invalid_run_options_response(options_json, &err.to_string()));
            }
        }
    };
    let report = start_run_source_with_flavor(source, parse_flavor(flavor_raw), fuel_config);
    leak_bytes(run_response_to_json(report))
}

#[cfg(feature = "runtime")]
#[unsafe(no_mangle)]
pub extern "C" fn debug_start_json(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
    options_ptr: u32,
    options_len: u32,
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_debug_response("source", &err)),
    };
    let flavor_raw = match std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_debug_response("flavor", &err)),
    };
    let fuel_config = if options_len == 0 {
        FuelConfig::default()
    } else {
        let options_json = match std::str::from_utf8(unpack_input(options_ptr, options_len)) {
            Ok(value) => value,
            Err(err) => return leak_bytes(invalid_utf8_debug_response("options", &err)),
        };
        match serde_json::from_str::<FuelConfig>(options_json) {
            Ok(value) => value,
            Err(err) => {
                return leak_bytes(invalid_debug_options_response(
                    options_json,
                    &err.to_string(),
                ));
            }
        }
    };
    let report = start_debug_source_with_flavor(source, parse_flavor(flavor_raw), fuel_config);
    leak_bytes(debug_response_to_json(report))
}

#[cfg(feature = "runtime")]
#[unsafe(no_mangle)]
pub extern "C" fn run_command_json(command_ptr: u32, command_len: u32) -> u64 {
    let command_json = match std::str::from_utf8(unpack_input(command_ptr, command_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_run_response("command", &err)),
    };
    let parsed = match serde_json::from_str::<RunCommand>(command_json) {
        Ok(value) => value,
        Err(err) => {
            return leak_bytes(invalid_run_command_response(command_json, &err.to_string()));
        }
    };
    let report = run_command(parsed);
    leak_bytes(run_response_to_json(report))
}

#[cfg(feature = "runtime")]
#[unsafe(no_mangle)]
pub extern "C" fn debug_command_json(command_ptr: u32, command_len: u32) -> u64 {
    let command_json = match std::str::from_utf8(unpack_input(command_ptr, command_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_debug_response("command", &err)),
    };
    let parsed = match serde_json::from_str::<DebugCommand>(command_json) {
        Ok(value) => value,
        Err(err) => {
            return leak_bytes(invalid_debug_command_response(
                command_json,
                &err.to_string(),
            ));
        }
    };
    let report = run_debug_command(parsed);
    leak_bytes(debug_response_to_json(report))
}

#[cfg(feature = "runtime")]
#[unsafe(no_mangle)]
pub extern "C" fn debug_state_json() -> u64 {
    leak_bytes(debug_response_to_json(debug_state()))
}

#[unsafe(no_mangle)]
pub extern "C" fn completion_catalog_json() -> u64 {
    leak_bytes(completion_catalog_to_json(build_completion_catalog()))
}

#[cfg(test)]
mod lint_tests {
    use std::path::Path;

    use super::parse_module_overrides;
    use crate::analyzer::{lint_source_with_flavor, lint_source_with_flavor_at_path};
    use serde_json::Value;
    use vm::{SourceFlavor, collect_inferred_local_type_hints};

    #[test]
    fn lint_accepts_bytes_literals_and_native_bytes_helpers() {
        let source = r#"
            use bytes;
            let payload = b"RSS\x00";
            let hex = bytes::to_hex(payload);
            let roundtrip = bytes::from_hex(hex);
            assert(roundtrip == payload);
            roundtrip;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.diagnostics.is_empty(),
            "expected bytes literal lint to pass, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn local_type_hint_json_payload_uses_hover_contract_fields() {
        let source = r#"
            fn plus_one(amount) {
                let total = amount + 1;
                total
            }

            plus_one(2);
        "#;

        let hints = collect_inferred_local_type_hints(source, SourceFlavor::RustScript)
            .expect("type hints should succeed");
        let payload: Value = serde_json::from_slice(&super::local_type_hints_to_json(hints))
            .expect("type hints should serialize as json");
        let items = payload["hints"]
            .as_array()
            .expect("hints should serialize as an array");

        let amount = items
            .iter()
            .find(|item| item["name"] == "amount")
            .expect("expected amount parameter hint");
        assert_eq!(amount["inferred_type"], "int");
        assert_eq!(amount["declared_line"], 2);
        assert_eq!(amount["last_line"], 3);

        let total = items
            .iter()
            .find(|item| item["name"] == "total")
            .expect("expected total local hint");
        assert_eq!(total["inferred_type"], "int");
        assert_eq!(total["declared_line"], 3);
        assert_eq!(total["last_line"], 4);
    }

    #[test]
    fn local_type_hints_support_embedded_stdlib_imports() {
        let source = r#"
            use stdlib::rss::strings as string;
            let label = 1;
            let value = string::trim("  hello  ");
            label;
        "#;

        let hints = super::local_type_hints_with_flavor(source, SourceFlavor::RustScript);
        let label = hints
            .iter()
            .find(|hint| hint.name == "label")
            .expect("expected a concrete hint alongside embedded stdlib imports");
        assert_eq!(label.inferred_type, "int");
        assert_eq!(label.declared_line, Some(3));
        assert_eq!(label.last_line, Some(5));

        let value = hints
            .iter()
            .find(|hint| hint.name == "value")
            .expect("expected a value hint with embedded stdlib imports");
        assert_eq!(value.declared_line, Some(4));
        assert_eq!(value.last_line, Some(4));
    }

    #[test]
    fn lint_avoids_unknown_warning_for_declared_json_decode_binding() {
        let source = r#"
            use json;
            struct Stats { score: int }
            struct Profile { stats: Stats }

            let payload_json = json::encode({});
            let payload_decoded: Profile = json::decode(payload_json);
            payload_decoded.stats.score;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("local 'payload_decoded'")),
            "declared schema binding should not surface an unknown-local warning, got {:?}",
            report.diagnostics
        );
        assert!(
            report.diagnostics.is_empty(),
            "annotated json decode example should lint cleanly, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_with_context_resolves_relative_imports_from_real_document_path() {
        let path = Path::new("workspace/examples/string_comp_test.rss");
        let source = r#"
            use super::stdlib::rss::strings::{trim};
            let values = trim("  two  ");
            values;
        "#;
        let mut options = parse_module_overrides(
            r#"[{"path":"workspace/stdlib/rss/strings.rss","source":"pub fn trim(value) { value }"}]"#,
        );
        options.set_module_override_source(
            "workspace/stdlib/rss/strings.rss",
            "pub fn trim(value) { value }",
        );

        let report =
            lint_source_with_flavor_at_path(source, path, SourceFlavor::RustScript, options);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity.as_str() == "warning"),
            "expected relative import lint to avoid hard errors, got {:?}",
            report.diagnostics
        );
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains("compiler could not determine the type of local 'values'")
            }),
            "expected relative import lint to surface the unknown local warning, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_with_context_keeps_unknown_local_warnings_with_relative_imports() {
        let path = Path::new("workspace/examples/string_comp_test.rss");
        let source = r#"
            use super::stdlib::rss::strings::{trim};
            let values = trim("  two  ");
            let arr = [1, "two"];
            let value = arr[0];
            value;
        "#;
        let mut options = parse_module_overrides(
            r#"[{"path":"workspace/stdlib/rss/strings.rss","source":"pub fn trim(value) { value }"}]"#,
        );
        options.set_module_override_source(
            "workspace/stdlib/rss/strings.rss",
            "pub fn trim(value) { value }",
        );

        let report =
            lint_source_with_flavor_at_path(source, path, SourceFlavor::RustScript, options);
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic.severity.as_str() == "warning"
                    && diagnostic
                        .message
                        .contains("compiler could not determine the type of local 'values'")
            }),
            "expected path-aware lint to keep the unknown-local warning, got {:?}",
            report.diagnostics
        );
    }
}

#[cfg(all(test, feature = "runtime"))]
mod runtime_tests {
    use std::time::Duration;

    use super::format_source_with_flavor;
    use crate::analyzer::{LintSeverity, lint_source_with_flavor};
    use crate::runtime::{
        DebugCommand, FuelConfig, RunCommand, debug_state, run_command, run_debug_command,
        run_source_with_flavor, start_debug_source_with_flavor, start_run_source_with_flavor,
    };
    use crate::stdlib::embedded_stdlib_compile_options;
    use vm::{
        CallOutcome, FunctionDecl, HostFunction, SourceFlavor, Value, Vm, VmStatus,
        compile_source_with_flavor_and_options,
    };

    fn rss_playground_examples() -> [(&'static str, &'static str); 6] {
        [
            ("Demo", include_str!("../examples/rss-complex-example.rss")),
            (
                "Callable Values Example",
                include_str!("../examples/rss-callable-values-example.rss"),
            ),
            (
                "IFFT Example",
                include_str!("../examples/rss-ifft-example.rss"),
            ),
            (
                "LRU Cache Example",
                include_str!("../examples/rss-lrucache-example.rss"),
            ),
            (
                "Collections and Iter Example",
                include_str!("../examples/rss-collections-iter-example.rss"),
            ),
            (
                "Strings and Regex Example",
                include_str!("../examples/rss-strings-regex-example.rss"),
            ),
        ]
    }

    fn rss_playground_example(name: &str) -> &'static str {
        rss_playground_examples()
            .into_iter()
            .find_map(|(example_name, source)| (example_name == name).then_some(source))
            .unwrap_or_else(|| panic!("missing playground example '{name}'"))
    }

    struct TestPrintFunction;

    impl HostFunction for TestPrintFunction {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            let values = match args {
                [] => vm::CallReturn::none(),
                [value] => vm::CallReturn::one(value.clone()),
                _ => vm::CallReturn::one(Value::array(args.to_vec())),
            };
            Ok(CallOutcome::Return(values))
        }
    }

    fn register_fixture_functions(vm: &mut Vm, functions: &[FunctionDecl]) {
        for decl in functions {
            match decl.name.as_str() {
                "print" => vm.bind_function("print", Box::new(TestPrintFunction)),
                "runtime::sleep" | "runtime::exit" => {}
                other => panic!("unknown fixture host function '{other}'"),
            }
        }
    }

    fn run_rss_fixture_without_jit(source: &str) -> Vec<Value> {
        let compiled = compile_source_with_flavor_and_options(
            source,
            SourceFlavor::RustScript,
            embedded_stdlib_compile_options(),
        )
        .expect("playground example should compile for runtime verification");
        let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
            .expect("fixture VM construction must not fail");
        let mut jit_config = *vm.jit_config();
        jit_config.enabled = false;
        vm.set_jit_config(jit_config);
        register_fixture_functions(&mut vm, &compiled.functions);

        loop {
            match vm.run().expect("fixture VM should run") {
                VmStatus::Halted => return vm.stack().to_vec(),
                VmStatus::Yielded => continue,
                VmStatus::Waiting(_op_id) => vm
                    .wait_for_host_op_blocking()
                    .expect("fixture VM should complete host operation"),
            }
        }
    }

    #[test]
    fn playground_rss_examples_are_formatted_lint_clean_and_runnable() {
        let options = embedded_stdlib_compile_options();

        for (name, source) in rss_playground_examples() {
            let formatted = format_source_with_flavor(source, SourceFlavor::RustScript)
                .expect("playground example should format");
            assert_eq!(
                formatted, source,
                "playground example '{name}' is not formatted"
            );

            compile_source_with_flavor_and_options(
                source,
                SourceFlavor::RustScript,
                options.clone(),
            )
            .unwrap_or_else(|err| panic!("playground example '{name}' should compile: {err}"));

            let lint = lint_source_with_flavor(source, SourceFlavor::RustScript);
            assert!(
                lint.diagnostics.is_empty(),
                "playground example '{name}' should be lint clean, got {:?}",
                lint.diagnostics
            );

            let _stack = run_rss_fixture_without_jit(source);
        }
    }

    #[test]
    fn playground_demo_example_lints_clean() {
        let lint =
            lint_source_with_flavor(rss_playground_example("Demo"), SourceFlavor::RustScript);
        assert!(
            lint.diagnostics.is_empty(),
            "playground example 'Demo' should be lint clean, got {:?}",
            lint.diagnostics
        );
    }

    #[test]
    fn playground_collections_and_iter_example_lints_clean() {
        let lint = lint_source_with_flavor(
            rss_playground_example("Collections and Iter Example"),
            SourceFlavor::RustScript,
        );
        assert!(
            lint.diagnostics.is_empty(),
            "playground example 'Collections and Iter Example' should be lint clean, got {:?}",
            lint.diagnostics
        );
    }

    #[test]
    fn run_reports_diagnostics_for_parse_errors() {
        let report = run_source_with_flavor("let value = ;", SourceFlavor::RustScript);
        assert!(report.error.is_some(), "expected parse error");
        assert!(
            !report.diagnostics.is_empty(),
            "expected lint diagnostics for parse error"
        );
    }

    #[test]
    fn lint_reports_structured_if_else_type_mismatch_diagnostics() {
        let source = r#"
            let value = if true => {
                1
            } else => {
                "x"
            };
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        let diagnostic = report
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.severity == LintSeverity::Error)
            .expect("expected a compile error diagnostic");
        assert!(diagnostic.line > 0, "expected a concrete diagnostic line");
        assert!(
            diagnostic.span.is_some(),
            "expected a full-line span for compile diagnostics"
        );
        assert_eq!(
            diagnostic.severity,
            LintSeverity::Error,
            "compile mismatches should surface as errors"
        );
        assert!(
            diagnostic
                .message
                .contains("if/else branches produced incompatible expression result"),
            "unexpected diagnostic message: {:?}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("int vs string"),
            "expected concrete type names in diagnostic: {:?}",
            diagnostic.message
        );
        assert!(
            !diagnostic.message.contains("IfElseBranchTypeMismatch"),
            "diagnostic should not expose raw debug formatting: {:?}",
            diagnostic.message
        );
        assert!(
            diagnostic.rendered.contains("let value = if true => {"),
            "expected rendered diagnostic snippet, got {:?}",
            diagnostic.rendered
        );
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == LintSeverity::Warning
                    && diagnostic
                        .message
                        .contains("compiler could not determine the type of local 'value'")
            }),
            "expected the unresolved local warning to be preserved, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn run_reports_callable_argument_type_mismatch_diagnostics() {
        let source = r#"
            use runtime;
            runtime::sleep("later");
        "#;

        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(report.error.is_some(), "expected compile failure");
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected a single callable type diagnostic"
        );
        let diagnostic = &report.diagnostics[0];
        assert!(
            diagnostic
                .message
                .contains("host function 'runtime::sleep' does not accept argument types"),
            "unexpected diagnostic message: {:?}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("string"),
            "expected actual argument type in diagnostic: {:?}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("ms: int"),
            "expected host parameter type annotation in diagnostic: {:?}",
            diagnostic.message
        );
    }

    #[test]
    fn lint_reports_trailing_function_return_semicolon_diagnostic() {
        let source = r#"
            fn addme(x) {
                x + x;
            }

            addme(1);
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected a single trailing-semicolon diagnostic"
        );
        let diagnostic = &report.diagnostics[0];
        assert!(diagnostic.line > 0, "expected a concrete diagnostic line");
        assert_eq!(
            diagnostic.severity,
            LintSeverity::Warning,
            "trailing semicolon lint should surface as a warning"
        );
        assert!(
            diagnostic
                .message
                .contains("function return expression should not end with ';'"),
            "unexpected diagnostic message: {:?}",
            diagnostic.message
        );
        assert!(
            diagnostic.span.is_some(),
            "expected a source span for the semicolon diagnostic"
        );
        assert!(
            diagnostic.rendered.contains("x + x;"),
            "expected rendered diagnostic snippet, got {:?}",
            diagnostic.rendered
        );
    }

    #[test]
    fn lint_accepts_bytes_literals_and_native_bytes_helpers() {
        let source = r#"
            use bytes;
            let payload = b"RSS\x00";
            let hex = bytes::to_hex(payload);
            let roundtrip = bytes::from_hex(hex);
            assert(roundtrip == payload);
            roundtrip;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.diagnostics.is_empty(),
            "expected bytes literal lint to pass, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_reports_inferred_unknown_local_types_as_warnings() {
        let source = r#"
            use stdlib::rss::strings as string;
            let value = string::trim("  two  ");
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected a single unknown local type warning"
        );
        let diagnostic = &report.diagnostics[0];
        assert_eq!(
            diagnostic.severity,
            LintSeverity::Warning,
            "unknown inferred local types should surface as warnings"
        );
        assert!(
            diagnostic
                .message
                .contains("compiler could not determine the type of local 'value'"),
            "unexpected diagnostic message: {:?}",
            diagnostic.message
        );
        let span = diagnostic
            .span
            .as_ref()
            .expect("unknown inferred local warning should expose a span");
        assert_eq!(
            span.start_line, 3,
            "warning should point at the declaration line"
        );
        assert!(
            span.end_col > span.start_col,
            "warning span should underline the declaration line"
        );
        assert!(
            diagnostic.rendered.contains("warning:")
                && diagnostic
                    .rendered
                    .contains("let value = string::trim(\"  two  \");"),
            "expected rendered warning snippet, got {:?}",
            diagnostic.rendered
        );
    }

    #[test]
    fn lint_respects_declared_schema_annotations_for_json_decode_bindings() {
        let source = r#"
            use json;
            struct Stats { score: int }
            struct Profile { stats: Stats }

            let payload_json = json::encode({});
            let payload_decoded: Profile = json::decode(payload_json);
            payload_decoded.stats.score;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("local 'payload_decoded'")),
            "declared schema binding should not surface an unknown-local warning, got {:?}",
            report.diagnostics
        );
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity != LintSeverity::Error),
            "annotated json decode example should not emit lint errors, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_reports_optional_usage_errors_and_keeps_concrete_types_after_handling() {
        let error_source = r#"
            struct Stats { score: int }
            struct Profile { stats: Stats }

            let profile: Profile = { stats: { score: 41 } };
            let score = profile?.stats?.score;
            score + 1;
        "#;

        let report = lint_source_with_flavor(error_source, SourceFlavor::RustScript);
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == LintSeverity::Error
                    && diagnostic
                        .message
                        .contains("optional value must be unwrapped before binary operation")
            }),
            "expected optional usage error, got {:?}",
            report.diagnostics
        );

        let ok_source = r#"
            struct Stats { score: int }
            struct Profile { stats: Stats }

            let profile: Profile = { stats: { score: 41 } };
            let score = profile?.stats?.score.unwrap_or(0);
            score + 1;
        "#;

        let report = lint_source_with_flavor(ok_source, SourceFlavor::RustScript);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("local 'score'")),
            "unwrap_or should keep 'score' concrete for lint, got {:?}",
            report.diagnostics
        );
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity != LintSeverity::Error),
            "unwrap_or example should not emit lint errors, got {:?}",
            report.diagnostics
        );

        let match_source = r#"
            struct Data { values: [int] }
            let data: Data = { values: [41] };
            let result = match data?.values?.[0] {
                None => 0,
                Some(value) => value + 1,
                _ => 0,
            };
            result;
        "#;

        let report = lint_source_with_flavor(match_source, SourceFlavor::RustScript);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity != LintSeverity::Error),
            "Some(value) match handling should keep the inner type concrete for lint, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn run_reports_unknown_local_warnings_after_successful_compile() {
        let source = r#"
            use stdlib::rss::strings as string;
            let value = string::trim("  ok  ");
            print("ok");
            value;
        "#;

        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(report.error.is_none(), "expected run to succeed");
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected successful run to keep unknown-local warnings"
        );
        assert_eq!(
            report.diagnostics[0].severity,
            LintSeverity::Warning,
            "run report should preserve warning severity"
        );
        assert!(
            report.diagnostics[0]
                .message
                .contains("compiler could not determine the type of local 'value'"),
            "unexpected warning: {:?}",
            report.diagnostics[0]
        );
    }

    #[test]
    fn lint_keeps_unknown_local_warnings_when_compile_errors_exist() {
        let source = r#"
            use runtime;
            use stdlib::rss::strings as string;
            let value = string::trim("  later  ");
            runtime::sleep("later");
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert_eq!(
            report.diagnostics.len(),
            2,
            "expected one warning and one compile error"
        );
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == LintSeverity::Warning
                    && diagnostic
                        .message
                        .contains("compiler could not determine the type of local 'value'")
            }),
            "expected the unknown-local warning to be preserved: {:?}",
            report.diagnostics
        );
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == LintSeverity::Error
                    && diagnostic.message.contains("runtime::sleep")
            }),
            "expected the compile error to be preserved: {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_keeps_unknown_local_warnings_with_leading_use_statements() {
        let source = r#"
            use runtime;
            use stdlib::rss::strings as string;
            let value = string::trim("  hello  ");
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected the unknown-local warning to survive leading use statements"
        );
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.severity, LintSeverity::Warning);
        assert!(
            diagnostic
                .message
                .contains("compiler could not determine the type of local 'value'"),
            "unexpected warning: {:?}",
            diagnostic
        );
        assert!(diagnostic.span.is_some(), "warning should expose a span");
    }

    #[test]
    fn lint_does_not_warn_for_if_else_block_locals_with_concrete_types() {
        let source = r#"
            use stdlib::rss::strings as string;

            let mut total = 0;

            let total = if !string::non_empty("rustscript") => {
                let zeroed = 0;
                zeroed
            } else => {
                let bumped = total + 1;
                bumped
            };
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.diagnostics.is_empty(),
            "block-local literals inside if/else expressions should not be flagged: {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_keeps_unknown_local_warnings_with_stdlib_use_alias() {
        let source = r#"
            use stdlib::rss::strings as string;
            let value = string::trim("  alias  ");
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected the unknown-local warning to survive stdlib use aliases"
        );
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.severity, LintSeverity::Warning);
        assert!(
            diagnostic
                .message
                .contains("compiler could not determine the type of local 'value'"),
            "unexpected warning: {:?}",
            diagnostic
        );
        assert!(diagnostic.span.is_some(), "warning should expose a span");
    }

    #[test]
    fn lint_supports_super_stdlib_use_alias_without_a_real_file_path() {
        let source = r#"
            use super::stdlib::rss::strings as string;
            let value = string::trim("  super  ");
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected pathless lint to resolve super::stdlib aliases without an io error: {:?}",
            report.diagnostics
        );
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.severity, LintSeverity::Warning);
        assert!(
            diagnostic
                .message
                .contains("compiler could not determine the type of local 'value'"),
            "unexpected warning: {:?}",
            diagnostic
        );
    }

    #[test]
    fn run_keeps_unknown_local_warnings_with_stdlib_use_alias() {
        let source = r#"
            use stdlib::rss::strings as string;
            let value = string::trim("  alias  ");
            value;
        "#;

        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(report.error.is_none(), "expected run to succeed");
        assert_eq!(
            report.diagnostics.len(),
            1,
            "expected successful run to keep the stdlib-alias unknown-local warning"
        );
        assert_eq!(report.diagnostics[0].severity, LintSeverity::Warning);
        assert!(
            report.diagnostics[0]
                .message
                .contains("compiler could not determine the type of local 'value'"),
            "unexpected warning: {:?}",
            report.diagnostics[0]
        );
    }

    #[test]
    fn lint_does_not_warn_for_callable_local_bindings() {
        let source = r#"
            let id = |x| x;
            let value = 1;
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.diagnostics.is_empty(),
            "callable locals should not be flagged as unknown types: {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn run_reports_missing_host_bindings() {
        let source = r#"
            fn custom(x);
            custom(1);
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(report.error.is_some(), "expected host binding error");
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|message| message.contains("no host binding")),
            "expected missing host binding message, got {:?}",
            report.error
        );
    }

    #[test]
    fn lint_accepts_embedded_stdlib_imports() {
        let source = r#"
            use stdlib::rss::strings as string;
            let value = string::trim("  hello  ");
            value;
        "#;
        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity == LintSeverity::Warning),
            "expected embedded stdlib import lint to emit warnings only, got {:?}",
            report.diagnostics
        );
        assert!(
            report.diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains("compiler could not determine the type of local 'value'")
            }),
            "expected embedded stdlib import lint to surface the unknown local warning, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn run_supports_embedded_stdlib_imports() {
        let source = r#"
            use stdlib::rss::strings as string;
            let value = string::replace("hi vm", "vm", "wasm");
            print(value);
            value;
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.error.is_none(),
            "expected run to succeed with embedded stdlib import, got {:?}",
            report.error
        );
        assert!(
            report.output.iter().any(|line| line == "hi wasm"),
            "expected output to include transformed string, got {:?}",
            report.output
        );
        assert!(
            report.stack.iter().any(|value| value == "hi wasm"),
            "expected stack to include transformed string, got {:?}",
            report.stack
        );
    }

    #[test]
    fn run_supports_embedded_stdlib_imports_with_named_runtime_host_import() {
        let source = r#"
            use stdlib::rss::strings as string;
            use runtime;
            runtime::sleep(0);
            let value = string::trim("  hello wasm  ");
            print(value);
            value;
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.error.is_none(),
            "expected run to succeed with embedded stdlib + named runtime import, got {:?}",
            report.error
        );
        assert!(
            report.output.iter().any(|line| line == "hello wasm"),
            "expected output to include trimmed string, got {:?}",
            report.output
        );
        assert!(
            report.stack.iter().any(|value| value == "hello wasm"),
            "expected stack to include trimmed string, got {:?}",
            report.stack
        );
    }

    #[test]
    fn lint_accepts_embedded_parse_stdlib_imports() {
        let source = r#"
            use stdlib::rss::parse as parse;
            let value: int = parse::parse_int_base_or("ff", 16, 0);
            value == 255;
        "#;
        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity == LintSeverity::Warning),
            "expected embedded parse stdlib lint to emit warnings only, got {:?}",
            report.diagnostics
        );
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn lint_accepts_json_and_regex_builtin_imports() {
        let source = r#"
            use re;
            use json;
            let matched = re::match("(?i)^rss$", "RSS");
            let payload = json::encode({ ok: matched });
            payload;
        "#;
        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.diagnostics.is_empty(),
            "expected json/re builtin lint to pass, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn run_supports_json_and_regex_builtins() {
        let source = r#"
            use re;
            use json;
            let matched = re::match("(?i)^rss$", "RSS");
            let payload = json::encode({ ok: matched });
            struct Payload { ok: bool }
            let decoded = json::decode::<Payload>(payload);
            let ok = decoded.ok.copy();
            if ok {
                print(1);
            } else {
                print(0);
            }
            ok;
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.error.is_none(),
            "expected run to succeed with json/re builtins, got {:?}",
            report.error
        );
        assert!(
            report.output.iter().any(|line| line == "1"),
            "expected output to include 1, got {:?}",
            report.output
        );
        assert!(
            report.stack.iter().any(|value| value == "true"),
            "expected stack to include true, got {:?}",
            report.stack
        );
    }

    #[test]
    fn run_supports_println_host_binding() {
        let source = r#"
            println("line");
            1;
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.error.is_none(),
            "expected run to succeed with println host binding, got {:?}",
            report.error
        );
        assert!(
            report.output.iter().any(|line| line == "line"),
            "expected output to include println line, got {:?}",
            report.output
        );
    }

    #[test]
    fn run_supports_mixed_print_call_arities_for_rustscript() {
        let source = r#"
            print(1);
            print("{}", 2);
            1;
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            report.error.is_none(),
            "expected run to succeed with mixed print arities, got {:?}",
            report.error
        );
        assert!(
            report.output.iter().any(|line| line == "1"),
            "expected output to include first print line, got {:?}",
            report.output
        );
        assert!(
            report.output.iter().any(|line| line == "2"),
            "expected output to include formatted print line, got {:?}",
            report.output
        );
    }

    #[test]
    fn run_source_supports_runtime_sleep_host_namespace() {
        let source = r#"
            use runtime;
            runtime::sleep(0);
            print("ok");
        "#;
        let report = run_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(report.error.is_none(), "expected runtime::sleep to succeed");
        assert!(report.halted, "program should halt");
        assert!(
            report.output.iter().any(|line| line == "ok"),
            "expected output to contain ok, got {:?}",
            report.output
        );
        assert!(
            report.stack.iter().any(|value| value == "ok"),
            "expected stack to contain ok, got {:?}",
            report.stack
        );
    }

    #[test]
    fn debug_session_supports_breakpoints_and_hover_print() {
        let source = r#"
            let mut value = 1;
            value = value + 2;
            print(value);
            value;
        "#;
        let start =
            start_debug_source_with_flavor(source, SourceFlavor::RustScript, FuelConfig::default());
        assert!(
            start.error.is_none(),
            "debug start should succeed, got {:?}",
            start.error
        );
        assert!(
            start.current_line.is_some(),
            "debug start should expose current line"
        );

        let set_break = run_debug_command(DebugCommand::BreakLine { line: 3 });
        assert!(
            !set_break.breakpoints.is_empty(),
            "expected at least one breakpoint, got {:?}",
            set_break.breakpoints
        );

        let cont = run_debug_command(DebugCommand::Continue);
        assert!(
            cont.current_line.is_some_and(|line| line >= 3),
            "continue should pause at or after the breakpoint line, got {:?}",
            cont.current_line
        );

        let hovered = run_debug_command(DebugCommand::PrintVar {
            name: "value".to_string(),
        });
        assert!(
            hovered.command_output.contains("value ="),
            "expected print_var to return a value, got {:?}",
            hovered.command_output
        );

        let stopped = run_debug_command(DebugCommand::Stop);
        assert!(stopped.halted, "stop should return halted=true");

        let state_after_stop = debug_state();
        assert!(
            state_after_stop.error.is_some(),
            "state should report inactive session after stop"
        );
    }

    #[test]
    fn run_session_can_resume_after_out_of_fuel() {
        let source = r#"
            let value = 1 + 1;
            print(value);
            value;
        "#;
        let start = start_run_source_with_flavor(
            source,
            SourceFlavor::RustScript,
            FuelConfig {
                fuel: Some(0),
                fuel_check_interval: Some(1),
                ..FuelConfig::default()
            },
        );
        assert!(start.error.is_none(), "run start should not error");
        assert!(
            start.yielded,
            "run should yield immediately when fuel is zero"
        );
        assert_eq!(start.fuel.remaining, Some(0));
        assert!(
            start.command_output.contains("out of fuel"),
            "expected out-of-fuel prompt, got {:?}",
            start.command_output
        );

        let add = run_command(RunCommand::AddFuel { amount: 16 });
        assert!(add.error.is_none(), "adding run fuel should not error");
        assert_eq!(add.fuel.remaining, Some(16));

        let resumed = run_command(RunCommand::Resume);
        assert!(resumed.error.is_none(), "resumed run should not error");
        assert!(resumed.halted, "run should halt after resuming");
        assert!(
            resumed.output.iter().any(|line| line == "2"),
            "expected resumed output to contain 2, got {:?}",
            resumed.output
        );
        assert!(
            resumed.stack.iter().any(|value| value == "2"),
            "expected resumed stack to contain 2, got {:?}",
            resumed.stack
        );
    }

    #[test]
    fn run_session_epoch_deadline_auto_rearms_on_resume() {
        let source = r#"
            let value = 1 + 1;
            print(value);
            value;
        "#;
        let start = start_run_source_with_flavor(
            source,
            SourceFlavor::RustScript,
            FuelConfig {
                mode: Some(crate::runtime::InterruptConfigMode::Epoch),
                epoch_deadline: Some(0),
                epoch_check_interval: Some(1),
                ..FuelConfig::default()
            },
        );
        assert!(start.error.is_none(), "run start should not error");
        assert!(
            start.yielded,
            "run should yield immediately at epoch deadline"
        );
        assert_eq!(start.fuel.mode, crate::runtime::InterruptModeState::Epoch);
        assert!(
            start.command_output.contains("epoch deadline reached"),
            "expected epoch pause prompt, got {:?}",
            start.command_output
        );

        let blocked_again = run_command(RunCommand::Resume);
        assert!(
            blocked_again.error.is_none(),
            "resuming without manual reconfiguration should not error"
        );
        assert!(
            blocked_again.yielded,
            "zero-length epoch deadline should auto re-arm and yield again"
        );
        assert!(
            blocked_again
                .command_output
                .contains("epoch deadline reached"),
            "expected epoch pause prompt after auto re-arm, got {:?}",
            blocked_again.command_output
        );

        let cleared = run_command(RunCommand::ClearEpochDeadline);
        assert!(
            cleared.error.is_none(),
            "clearing epoch deadline should not error"
        );
        assert_eq!(cleared.fuel.mode, crate::runtime::InterruptModeState::None);

        let resumed = run_command(RunCommand::Resume);
        assert!(resumed.error.is_none(), "resumed run should not error");
        assert!(resumed.halted, "run should halt after resuming");
        assert!(
            resumed.output.iter().any(|line| line == "2"),
            "expected resumed output to contain 2, got {:?}",
            resumed.output
        );
    }

    #[test]
    fn run_session_polls_runtime_sleep_until_ready() {
        let source = r#"
            use runtime;
            runtime::sleep(25);
            print("awake");
            "awake";
        "#;

        let start =
            start_run_source_with_flavor(source, SourceFlavor::RustScript, FuelConfig::default());
        assert!(start.error.is_none(), "run start should not error");
        assert!(!start.halted, "run should remain active while sleeping");
        assert!(
            !start.yielded,
            "sleep wait should not look like a fuel yield"
        );
        assert!(
            start.command_output.contains("runtime::sleep pending"),
            "expected pending sleep message, got {:?}",
            start.command_output
        );

        let pending = run_command(RunCommand::Resume);
        assert!(pending.error.is_none(), "resume poll should not error");
        assert!(
            !pending.halted,
            "sleep should still be active on immediate poll"
        );
        assert!(
            pending.command_output.contains("runtime::sleep pending"),
            "expected pending sleep message, got {:?}",
            pending.command_output
        );

        std::thread::sleep(Duration::from_millis(35));

        let resumed = run_command(RunCommand::Resume);
        assert!(resumed.error.is_none(), "resumed run should not error");
        assert!(resumed.halted, "run should halt after sleep completes");
        assert!(
            resumed.output.iter().any(|line| line == "awake"),
            "expected resumed output to contain awake, got {:?}",
            resumed.output
        );
        assert!(
            resumed.stack.iter().any(|value| value == "awake"),
            "expected resumed stack to contain awake, got {:?}",
            resumed.stack
        );
    }

    #[test]
    fn debug_session_reports_and_updates_fuel() {
        let source = r#"
            let mut value = 1;
            value = value + 2;
            print(value);
            value;
        "#;
        let start = start_debug_source_with_flavor(
            source,
            SourceFlavor::RustScript,
            FuelConfig {
                fuel: Some(0),
                fuel_check_interval: Some(2),
                ..FuelConfig::default()
            },
        );
        assert!(start.error.is_none(), "debug start should succeed");
        assert_eq!(start.fuel.remaining, Some(0));
        assert_eq!(start.fuel.check_interval, 2);

        let blocked = run_debug_command(DebugCommand::Continue);
        assert!(blocked.error.is_none(), "continue should pause, not error");
        assert_eq!(blocked.fuel.remaining, Some(0));
        assert!(
            blocked.command_output.contains("out of fuel"),
            "expected out-of-fuel pause, got {:?}",
            blocked.command_output
        );

        let add = run_debug_command(DebugCommand::AddFuel { amount: 64 });
        assert!(add.error.is_none(), "fuel add should succeed");
        assert!(
            add.fuel.remaining.is_some_and(|remaining| remaining >= 63),
            "expected substantial fuel after top-up, got {:?}",
            add.fuel.remaining
        );
        assert!(
            add.command_output.contains("fuel added: 64"),
            "expected fuel add output, got {:?}",
            add.command_output
        );

        let interval = run_debug_command(DebugCommand::SetFuelCheckInterval { interval: 1 });
        assert!(interval.error.is_none(), "interval update should succeed");
        assert_eq!(interval.fuel.check_interval, 1);

        let resumed = run_debug_command(DebugCommand::Continue);
        assert!(
            resumed.error.is_none(),
            "resumed debug run should not error"
        );
        assert!(resumed.halted, "resumed debug run should halt");
        assert!(
            resumed.output.iter().any(|line| line == "3"),
            "expected debug output to contain 3, got {:?}",
            resumed.output
        );
    }

    #[test]
    fn debug_session_reports_and_updates_epoch() {
        let source = r#"
            let mut value = 1;
            value = value + 2;
            print(value);
            value;
        "#;
        let start = start_debug_source_with_flavor(
            source,
            SourceFlavor::RustScript,
            FuelConfig {
                mode: Some(crate::runtime::InterruptConfigMode::Epoch),
                epoch_deadline: Some(0),
                epoch_check_interval: Some(2),
                ..FuelConfig::default()
            },
        );
        assert!(start.error.is_none(), "debug start should succeed");
        assert_eq!(start.fuel.mode, crate::runtime::InterruptModeState::Epoch);
        assert_eq!(start.fuel.check_interval, 2);

        let blocked = run_debug_command(DebugCommand::Continue);
        assert!(blocked.error.is_none(), "continue should pause, not error");
        assert!(
            blocked.command_output.contains("epoch deadline reached"),
            "expected epoch pause, got {:?}",
            blocked.command_output
        );

        let ticked = run_debug_command(DebugCommand::TickEpoch { amount: 3 });
        assert!(ticked.error.is_none(), "epoch tick should succeed");
        assert!(
            ticked.command_output.contains("epoch advanced by 3"),
            "expected epoch tick output, got {:?}",
            ticked.command_output
        );

        let interval = run_debug_command(DebugCommand::SetEpochCheckInterval { interval: 1 });
        assert!(
            interval.error.is_none(),
            "epoch interval update should succeed"
        );
        assert_eq!(interval.fuel.check_interval, 1);

        let blocked_again = run_debug_command(DebugCommand::Continue);
        assert!(
            blocked_again.error.is_none(),
            "continue should auto re-arm the epoch deadline"
        );
        assert_eq!(blocked_again.fuel.epoch_current, 3);
        assert_eq!(blocked_again.fuel.epoch_deadline, Some(3));
        assert!(
            blocked_again
                .command_output
                .contains("epoch deadline reached"),
            "expected repeated epoch pause after auto re-arm, got {:?}",
            blocked_again.command_output
        );

        let cleared = run_debug_command(DebugCommand::ClearEpochDeadline);
        assert!(
            cleared.error.is_none(),
            "clearing epoch deadline should succeed"
        );

        let resumed = run_debug_command(DebugCommand::Continue);
        assert!(
            resumed.error.is_none(),
            "resumed debug run should not error"
        );
        assert!(resumed.halted, "resumed debug run should halt");
        assert!(
            resumed.output.iter().any(|line| line == "3"),
            "expected debug output to contain 3, got {:?}",
            resumed.output
        );
    }

    #[test]
    fn debug_session_continue_rearms_epoch_deadline_relative_to_current_epoch() {
        let source = r#"
            let mut value = 1;
            value = value + 2;
            print(value);
            value;
        "#;
        let start = start_debug_source_with_flavor(
            source,
            SourceFlavor::RustScript,
            FuelConfig {
                mode: Some(crate::runtime::InterruptConfigMode::Epoch),
                epoch_deadline: Some(1),
                epoch_check_interval: Some(1),
                ..FuelConfig::default()
            },
        );
        assert!(start.error.is_none(), "debug start should succeed");

        let ticked = run_debug_command(DebugCommand::TickEpoch { amount: 1 });
        assert!(ticked.error.is_none(), "initial epoch tick should succeed");
        assert_eq!(ticked.fuel.epoch_current, 1);
        assert_eq!(ticked.fuel.epoch_deadline, Some(1));

        let blocked = run_debug_command(DebugCommand::Continue);
        assert!(
            blocked.error.is_none(),
            "continue should pause at the first epoch deadline"
        );
        assert_eq!(blocked.fuel.epoch_current, 1);
        assert_eq!(blocked.fuel.epoch_deadline, Some(1));
        assert!(
            blocked.command_output.contains("epoch deadline reached"),
            "expected initial epoch pause, got {:?}",
            blocked.command_output
        );

        let advanced = run_debug_command(DebugCommand::TickEpoch { amount: 5 });
        assert!(
            advanced.error.is_none(),
            "epoch tick while paused should succeed"
        );
        assert_eq!(advanced.fuel.epoch_current, 6);
        assert_eq!(advanced.fuel.epoch_deadline, Some(1));

        let resumed = run_debug_command(DebugCommand::Continue);
        assert!(
            resumed.error.is_none(),
            "continue should re-arm the epoch deadline relative to the current epoch"
        );
        assert!(resumed.halted, "program should finish after re-arming");
        assert_eq!(resumed.fuel.epoch_current, 6);
        assert_eq!(resumed.fuel.epoch_deadline, Some(7));
        assert!(
            resumed.output.iter().any(|line| line == "3"),
            "expected debug output to contain 3, got {:?}",
            resumed.output
        );
    }

    #[test]
    fn debug_session_pauses_for_runtime_sleep_without_error() {
        let source = r#"
            use runtime;
            runtime::sleep(25);
            print(7);
            7;
        "#;

        let start =
            start_debug_source_with_flavor(source, SourceFlavor::RustScript, FuelConfig::default());
        assert!(start.error.is_none(), "debug start should succeed");

        let waiting = run_debug_command(DebugCommand::Continue);
        assert!(waiting.error.is_none(), "sleep wait should not error");
        assert!(
            !waiting.halted,
            "debug session should stay active while sleeping"
        );
        assert!(
            waiting.command_output.contains("runtime::sleep pending"),
            "expected pending sleep message, got {:?}",
            waiting.command_output
        );

        std::thread::sleep(Duration::from_millis(35));

        let resumed = run_debug_command(DebugCommand::Continue);
        assert!(
            resumed.error.is_none(),
            "resumed debug run should not error"
        );
        assert!(
            resumed.halted,
            "debug run should halt after sleep completes"
        );
        assert!(
            resumed.output.iter().any(|line| line == "7"),
            "expected debug output to contain 7, got {:?}",
            resumed.output
        );
    }

    #[test]
    fn wasm_run_response_serializes_structured_error_fields_alongside_message() {
        let report = run_source_with_flavor("let =", SourceFlavor::RustScript);
        assert_eq!(report.error_code.as_deref(), Some("source_error"));
        assert!(report.error.is_some());
        let payload = super::run_response_to_json(report);
        let json: serde_json::Value = serde_json::from_slice(&payload).expect("run JSON");
        assert_eq!(json["error_code"], "source_error");
        assert_eq!(json["error_details"]["operation"], "source");
        assert!(json["error_details"]["message"].is_string());
        assert!(json["error"].is_string());
    }
}

#[cfg(all(test, feature = "runtime"))]
mod fallback_tests {
    use serde_json::Value;

    use super::{
        LintDiagnosticJson, LintSpanJson, RunResponse, invalid_run_command_response,
        invalid_run_options_response, invalid_utf8_run_response, lint_diagnostic_to_json,
        run_response_fallback, serialize_run_response,
    };
    use crate::runtime::{
        FuelState, InterruptModeState, RunErrorDetails, RunReport, run_source_with_flavor,
    };
    use vm::SourceFlavor;

    fn disabled_fuel_json() -> crate::runtime::FuelState {
        FuelState {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval: 1,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }
    }

    fn report_with_details(message: &str, code: &str, operation: &str) -> RunReport {
        RunReport {
            diagnostics: Vec::new(),
            output: Vec::new(),
            stack: Vec::new(),
            error: Some(message.to_string()),
            error_code: Some(code.to_string()),
            error_details: Some(RunErrorDetails {
                operation: operation.to_string(),
                message: message.to_string(),
                limit: None,
                value: None,
            }),
            halted: true,
            yielded: false,
            fuel: disabled_fuel_json(),
            command_output: String::new(),
        }
    }

    fn structured_report() -> RunReport {
        // A realistic VM failure carrying a stable machine code and a
        // structured detail payload (operation, limit, value).
        RunReport::runtime_error(
            "resource arena identity space is exhausted".to_string(),
            Vec::new(),
            Vec::new(),
            disabled_fuel_json(),
        )
    }

    fn response_for(report: RunReport) -> RunResponse {
        let ok = report.error.is_none();
        RunResponse {
            ok,
            diagnostics: report
                .diagnostics
                .into_iter()
                .map(lint_diagnostic_to_json)
                .collect(),
            output: report.output,
            stack: report.stack,
            error: report.error,
            error_code: report.error_code,
            error_details: report.error_details,
            halted: report.halted,
            yielded: report.yielded,
            command_output: report.command_output,
            fuel: super::fuel_state_to_json(report.fuel),
        }
    }

    /// A [`RunResponse`] whose diagnostics carry non-trivial content: quotes,
    /// backslashes, control characters, Unicode, and a mix of span presence.
    /// This is the payload that forces `lint_diagnostic_json_to_value` (and
    /// its nested span reconstruction) to actually run in the fallback.
    fn populated_response() -> RunResponse {
        RunResponse {
            ok: false,
            diagnostics: vec![
                LintDiagnosticJson {
                    line: 7,
                    severity: "error",
                    message: "unterminated string literal \"oops\\n\"".to_string(),
                    span: Some(LintSpanJson {
                        start_line: 7,
                        start_col: 3,
                        end_line: 9,
                        end_col: 41,
                    }),
                    rendered: "  --> line 7: unterminated \"quote\\\" \\\\ path\"".to_string(),
                },
                LintDiagnosticJson {
                    line: 12,
                    severity: "warning",
                    message: "unused variable `café_中`\u{0} (nul)".to_string(),
                    span: None,
                    rendered: "  = note: `café_中` never used \\\\ backslash".to_string(),
                },
            ],
            output: vec!["line \"quoted\"".to_string(), "tab\there".to_string()],
            stack: vec!["at main (café_中)".to_string()],
            error: Some("compile failed: \"syntax\" \\\\ path".to_string()),
            error_code: Some("source_error".to_string()),
            error_details: Some(RunErrorDetails {
                operation: "source".to_string(),
                message: "compile failed: \"syntax\" \\\\ path".to_string(),
                limit: Some(12),
                value: Some(0x1f600),
            }),
            halted: true,
            yielded: false,
            command_output: "cmd \"echo\" \\\\ done".to_string(),
            fuel: super::fuel_state_to_json(disabled_fuel_json()),
        }
    }

    #[test]
    fn run_response_fallback_matches_serializer_with_populated_diagnostics() {
        // The fallback's most drift-prone component is the manual
        // field-by-field JSON reconstruction of each diagnostic, including the
        // nested span. Exercise it with real, non-trivial content: one
        // diagnostic with a full span and one without a span, carrying quotes,
        // backslashes, control characters and Unicode in every string field.
        let response = populated_response();
        let expected: Value =
            serde_json::from_slice(&serde_json::to_vec(&response).expect("full serialize"))
                .expect("expected json");
        let fallback: Value =
            serde_json::from_slice(&run_response_fallback(&response)).expect("fallback json");

        assert_eq!(
            fallback, expected,
            "fallback must be byte-parity with the serializer"
        );
        assert_eq!(fallback["diagnostics"].as_array().map(Vec::len), Some(2));

        // The populated span survives reconstruction with exact coordinates.
        let with_span = &fallback["diagnostics"][0];
        assert_eq!(with_span["line"], 7);
        assert_eq!(with_span["severity"], "error");
        assert_eq!(with_span["span"]["start_line"], 7);
        assert_eq!(with_span["span"]["start_col"], 3);
        assert_eq!(with_span["span"]["end_line"], 9);
        assert_eq!(with_span["span"]["end_col"], 41);

        // The span-less diagnostic keeps `span: null`, never a dropped field.
        let without_span = &fallback["diagnostics"][1];
        assert_eq!(without_span["line"], 12);
        assert_eq!(without_span["span"], Value::Null);
        assert!(without_span["rendered"].as_str().unwrap().contains("\\"));
        assert!(without_span["rendered"].as_str().unwrap().contains('中'));
    }

    #[test]
    fn run_response_fallback_preserves_structured_error_fields_exactly() {
        let report = structured_report();
        let response = response_for(report);
        let expected: Value =
            serde_json::from_slice(&serde_json::to_vec(&response).expect("full serialize"))
                .expect("expected json");
        let fallback: Value =
            serde_json::from_slice(&run_response_fallback(&response)).expect("fallback json");

        assert_eq!(fallback, expected);
        assert_eq!(
            fallback["error"],
            "resource arena identity space is exhausted"
        );
        assert_eq!(fallback["error_code"], "runtime_error");
        assert_eq!(fallback["error_details"]["operation"], "runtime");
        assert_eq!(
            fallback["error_details"]["message"],
            "resource arena identity space is exhausted"
        );
        assert_eq!(fallback["halted"], true);
        assert_eq!(fallback["fuel"]["enabled"], false);
    }

    #[test]
    fn run_response_fallback_keeps_arena_operation_and_legacy_codes_distinguishable() {
        // Distinct structured detail payloads (arena, modern operation tag,
        // legacy runtime code) must survive the fallback unchanged and remain
        // distinguishable from each other.
        let arena = report_with_details(
            "resource arena identity space is exhausted",
            "resource_arena_id_exhausted",
            "resource::table",
        );
        let operation = report_with_details(
            "operation registry tag identity space is exhausted",
            "operation_registry_tag_exhausted",
            "vm::operation_registry",
        );
        let legacy = report_with_details(
            "legacy resource identity space is exhausted",
            "legacy_runtime_resource_id_exhausted",
            "legacy::resource_arena",
        );

        let arena_json: Value =
            serde_json::from_slice(&run_response_fallback(&response_for(arena))).unwrap();
        let operation_json: Value =
            serde_json::from_slice(&run_response_fallback(&response_for(operation))).unwrap();
        let legacy_json: Value =
            serde_json::from_slice(&run_response_fallback(&response_for(legacy))).unwrap();

        assert_eq!(arena_json["error_code"], "resource_arena_id_exhausted");
        assert_eq!(
            operation_json["error_code"],
            "operation_registry_tag_exhausted"
        );
        assert_eq!(
            legacy_json["error_code"],
            "legacy_runtime_resource_id_exhausted"
        );
        assert_eq!(arena_json["error_details"]["operation"], "resource::table");
        assert_eq!(
            operation_json["error_details"]["operation"],
            "vm::operation_registry"
        );
        assert_eq!(
            legacy_json["error_details"]["operation"],
            "legacy::resource_arena"
        );
        assert_ne!(
            arena_json["error_details"]["operation"],
            operation_json["error_details"]["operation"]
        );
        assert_ne!(
            operation_json["error_details"]["operation"],
            legacy_json["error_details"]["operation"]
        );
        assert_ne!(
            arena_json["error_details"]["operation"],
            legacy_json["error_details"]["operation"]
        );
    }

    #[test]
    fn run_response_fallback_never_drops_limit_and_value_details() {
        let report = RunReport {
            diagnostics: Vec::new(),
            output: Vec::new(),
            stack: Vec::new(),
            error: Some("resource arena identity space is exhausted".to_string()),
            error_code: Some("resource_arena_id_exhausted".to_string()),
            error_details: Some(RunErrorDetails {
                operation: "resource::table".to_string(),
                message: "resource arena identity space is exhausted".to_string(),
                limit: Some(0x00ff_ffff),
                value: Some(0x0100_0000),
            }),
            halted: true,
            yielded: false,
            fuel: disabled_fuel_json(),
            command_output: String::new(),
        };
        let json: Value =
            serde_json::from_slice(&run_response_fallback(&response_for(report))).unwrap();
        assert_eq!(json["error_details"]["limit"], 0x00ff_ffffu64);
        assert_eq!(json["error_details"]["value"], 0x0100_0000u64);
    }

    #[test]
    fn all_run_response_sites_preserve_error_fields_through_shared_fallback() {
        // Normal path: run response with a structured VM error.
        let run = run_source_with_flavor("let =", SourceFlavor::RustScript);
        let run_json: Value =
            serde_json::from_slice(&super::run_response_to_json(run)).expect("run json");
        assert_eq!(run_json["error_code"], "source_error");
        assert!(run_json["error_details"]["operation"].is_string());

        // Invalid utf-8 run response.
        let bad = String::from_utf8(vec![0xff])
            .expect_err("invalid utf-8 produced at runtime")
            .utf8_error();
        let utf8_json: Value =
            serde_json::from_slice(&invalid_utf8_run_response("source", &bad)).expect("utf8 json");
        assert_eq!(utf8_json["error_code"], "input_error");
        assert_eq!(utf8_json["error_details"]["operation"], "wasm::input");
        assert!(
            utf8_json["error"]
                .as_str()
                .unwrap()
                .contains("invalid utf-8")
        );

        // Invalid run command response.
        let command_json: Value =
            serde_json::from_slice(&invalid_run_command_response("{}", "boom")).expect("cmd json");
        assert_eq!(command_json["error_code"], "input_error");
        assert_eq!(
            command_json["error_details"]["operation"],
            "wasm::run_command"
        );
        assert!(
            command_json["error"]
                .as_str()
                .unwrap()
                .contains("invalid run command")
        );

        // Invalid run options response.
        let options_json: Value =
            serde_json::from_slice(&invalid_run_options_response("{}", "boom")).expect("opts json");
        assert_eq!(options_json["error_code"], "input_error");
        assert_eq!(
            options_json["error_details"]["operation"],
            "wasm::run_options"
        );
        assert!(
            options_json["error"]
                .as_str()
                .unwrap()
                .contains("invalid run options")
        );
    }

    #[test]
    fn serialize_run_response_matches_shared_serializer_for_structured_errors() {
        // `serialize_run_response` first tries the normal struct serializer.
        // For plain serializable data that path succeeds, so this asserts the
        // public helper's *normal* output equals the struct serializer — it
        // does not (and cannot) force the fallback. Fallback parity itself is
        // covered by the direct `run_response_fallback` tests above.
        let report = structured_report();
        let response = response_for(report);
        let payload = serialize_run_response(&response);
        let json: Value = serde_json::from_slice(&payload).expect("serialized json");
        assert_eq!(
            json,
            serde_json::to_value(&response).expect("struct serialization"),
            "normal path must equal the struct serializer"
        );
        assert_eq!(json["error_code"], "runtime_error");
        assert_eq!(json["error_details"]["operation"], "runtime");
        assert!(json["error"].is_string());
    }
}
