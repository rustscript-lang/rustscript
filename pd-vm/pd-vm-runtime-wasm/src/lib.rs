mod analyzer;
mod completions;
mod runtime;
mod stdlib;

use serde::Serialize;
use vm::SourceFlavor;

use crate::analyzer::{LintDiagnostic, LintReport, LintSpan, lint_source_with_flavor};
use crate::completions::{CompletionCatalog, build_completion_catalog};
use crate::runtime::{
    DebugCommand, DebugReport, FuelConfig, FuelState, RunCommand, RunReport, debug_state,
    run_command, run_debug_command, start_debug_source_with_flavor, start_run_source_with_flavor,
};

#[derive(Serialize)]
struct LintResponse {
    diagnostics: Vec<LintDiagnosticJson>,
}

#[derive(Serialize)]
struct LintDiagnosticJson {
    line: usize,
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
struct RunResponse {
    ok: bool,
    diagnostics: Vec<LintDiagnosticJson>,
    output: Vec<String>,
    stack: Vec<String>,
    error: Option<String>,
    halted: bool,
    yielded: bool,
    command_output: String,
    fuel: FuelStateJson,
}

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

#[derive(Serialize)]
struct FuelStateJson {
    enabled: bool,
    remaining: Option<u64>,
    check_interval: u32,
}

fn parse_flavor(raw: &str) -> SourceFlavor {
    match raw.trim().to_ascii_lowercase().as_str() {
        "javascript" | "js" => SourceFlavor::JavaScript,
        "lua" => SourceFlavor::Lua,
        "scheme" | "scm" => SourceFlavor::Scheme,
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
        halted: report.halted,
        yielded: report.yielded,
        command_output: report.command_output,
        fuel: fuel_state_to_json(report.fuel),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"diagnostics\":[],\"output\":[],\"stack\":[],\"halted\":true,\"yielded\":false,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

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

fn fuel_state_to_json(fuel: FuelState) -> FuelStateJson {
    FuelStateJson {
        enabled: fuel.enabled,
        remaining: fuel.remaining,
        check_interval: fuel.check_interval,
    }
}

fn completion_catalog_to_json(catalog: CompletionCatalog) -> Vec<u8> {
    serde_json::to_vec(&catalog).unwrap_or_else(|_| {
        b"{\"rustscript\":[],\"javascript\":[],\"lua\":[],\"scheme\":[]}".to_vec()
    })
}

fn invalid_utf8_lint_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = LintResponse {
        diagnostics: vec![LintDiagnosticJson {
            line: 1,
            message: format!("invalid utf-8 {label}: {err}"),
            span: None,
            rendered: format!("invalid utf-8 {label}: {err}"),
        }],
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec())
}

fn invalid_utf8_run_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = RunResponse {
        ok: false,
        diagnostics: vec![LintDiagnosticJson {
            line: 1,
            message: format!("invalid utf-8 {label}: {err}"),
            span: None,
            rendered: format!("invalid utf-8 {label}: {err}"),
        }],
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!("invalid utf-8 {label}: {err}")),
        halted: true,
        yielded: false,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            remaining: None,
            check_interval: 1,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"diagnostics\":[],\"output\":[],\"stack\":[],\"halted\":true,\"yielded\":false,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

fn invalid_utf8_debug_response(label: &str, err: &std::str::Utf8Error) -> Vec<u8> {
    let response = DebugResponse {
        diagnostics: vec![LintDiagnosticJson {
            line: 1,
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
            remaining: None,
            check_interval: 1,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"diagnostics\":[],\"output\":[],\"stack\":[],\"breakpoints\":[],\"halted\":true,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

fn invalid_run_command_response(command_json: &str, error: &str) -> Vec<u8> {
    let response = RunResponse {
        ok: false,
        diagnostics: Vec::new(),
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!(
            "invalid run command: {error}; payload={command_json}"
        )),
        halted: true,
        yielded: false,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            remaining: None,
            check_interval: 1,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"diagnostics\":[],\"output\":[],\"stack\":[],\"halted\":true,\"yielded\":false,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

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
            remaining: None,
            check_interval: 1,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"diagnostics\":[],\"output\":[],\"stack\":[],\"breakpoints\":[],\"halted\":true,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

fn invalid_run_options_response(options_json: &str, error: &str) -> Vec<u8> {
    let response = RunResponse {
        ok: false,
        diagnostics: Vec::new(),
        output: Vec::new(),
        stack: Vec::new(),
        error: Some(format!(
            "invalid run options: {error}; payload={options_json}"
        )),
        halted: true,
        yielded: false,
        command_output: String::new(),
        fuel: fuel_state_to_json(FuelState {
            enabled: false,
            remaining: None,
            check_interval: 1,
        }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"diagnostics\":[],\"output\":[],\"stack\":[],\"halted\":true,\"yielded\":false,\"command_output\":\"\",\"fuel\":{\"enabled\":false,\"remaining\":null,\"check_interval\":1}}".to_vec()
    })
}

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
            remaining: None,
            check_interval: 1,
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

#[unsafe(no_mangle)]
pub extern "C" fn debug_state_json() -> u64 {
    leak_bytes(debug_response_to_json(debug_state()))
}

#[unsafe(no_mangle)]
pub extern "C" fn completion_catalog_json() -> u64 {
    leak_bytes(completion_catalog_to_json(build_completion_catalog()))
}

#[cfg(test)]
mod tests {
    use super::parse_flavor;
    use crate::analyzer::lint_source_with_flavor;
    use crate::completions::build_completion_catalog;
    use crate::runtime::{
        DebugCommand, FuelConfig, RunCommand, debug_state, run_command, run_debug_command,
        run_source_with_flavor, start_debug_source_with_flavor, start_run_source_with_flavor,
    };
    use vm::SourceFlavor;

    #[test]
    fn parse_flavor_accepts_aliases() {
        assert_eq!(parse_flavor("js"), SourceFlavor::JavaScript);
        assert_eq!(parse_flavor("scm"), SourceFlavor::Scheme);
        assert_eq!(parse_flavor("lua"), SourceFlavor::Lua);
        assert_eq!(parse_flavor("rss"), SourceFlavor::RustScript);
    }

    #[test]
    fn lint_reports_no_errors_for_all_supported_frontends() {
        let cases = [
            (
                SourceFlavor::RustScript,
                include_str!("../../examples/example.rss"),
            ),
            (
                SourceFlavor::JavaScript,
                include_str!("../../examples/example.js"),
            ),
            (SourceFlavor::Lua, "local a = 1\na = a + 1\na"),
            (SourceFlavor::Scheme, "(define a 1)\n(set! a (+ a 1))\na"),
        ];

        for (flavor, source) in cases {
            let report = lint_source_with_flavor(source, flavor);
            assert!(
                report.diagnostics.is_empty(),
                "lint should succeed for {flavor:?}, got diagnostics: {:?}",
                report.diagnostics
            );
        }
    }

    #[test]
    fn run_returns_output_for_all_supported_frontends() {
        let cases = [
            (SourceFlavor::RustScript, "print(1 + 1);"),
            (SourceFlavor::JavaScript, "console.log(1 + 1);"),
            (SourceFlavor::Lua, "print(1 + 1)"),
            (SourceFlavor::Scheme, "(print (+ 1 1))"),
        ];

        for (flavor, source) in cases {
            let report = run_source_with_flavor(source, flavor);
            assert!(
                report.error.is_none(),
                "run should succeed for {flavor:?}, got error: {:?}",
                report.error
            );
            assert!(
                report.output.iter().any(|line| line == "2"),
                "expected output to contain '2' for {flavor:?}, got {:?}",
                report.output
            );
            assert!(
                report.stack.iter().any(|value| value == "2"),
                "expected stack to contain '2' for {flavor:?}, got {:?}",
                report.stack
            );
        }
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
            report.diagnostics.is_empty(),
            "expected embedded stdlib import lint to pass, got {:?}",
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
            use runtime::{sleep};
            sleep(0);
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
    fn lint_accepts_json_and_regex_builtin_imports() {
        let source = r#"
            use re;
            use json;
            let matched = re::match("^rss$", "RSS", "i");
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
            let matched = re::match("^rss$", "RSS", "i");
            let payload = json::encode({ ok: matched });
            let decoded = json::decode(payload);
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
    fn completion_catalog_reports_stdlib_and_host_entries() {
        let catalog = build_completion_catalog();
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "string::trim"),
            "expected RustScript stdlib completion entry"
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "json::encode"),
            "expected RustScript json namespace completion entry"
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "re::match"),
            "expected RustScript regex namespace completion entry"
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "runtime::sleep"),
            "expected RustScript runtime host namespace completion entry"
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
}
