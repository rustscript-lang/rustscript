mod analyzer;
mod completions;
mod runtime;
mod stdlib;

use serde::Serialize;
use vm::SourceFlavor;

use crate::analyzer::{LintDiagnostic, LintReport, LintSpan, lint_source_with_flavor};
use crate::completions::{CompletionCatalog, build_completion_catalog};
use crate::runtime::{RunReport, run_source_with_flavor};

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
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"diagnostics\":[],\"output\":[],\"stack\":[]}".to_vec()
    })
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
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"ok\":false,\"diagnostics\":[],\"output\":[],\"stack\":[]}".to_vec()
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
) -> u64 {
    let source = match std::str::from_utf8(unpack_input(source_ptr, source_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_run_response("source", &err)),
    };
    let flavor_raw = match std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_run_response("flavor", &err)),
    };
    let report = run_source_with_flavor(source, parse_flavor(flavor_raw));
    leak_bytes(run_response_to_json(report))
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
    use crate::runtime::run_source_with_flavor;
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
                !report.has_errors(),
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
            !report.has_errors(),
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
            !report.has_errors(),
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
                .any(|entry| entry.label == "add_one"),
            "expected RustScript host completion entry"
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
    }
}
