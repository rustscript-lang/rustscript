mod analyzer;
mod completions;
mod stdlib;

use std::path::Path;

use serde::{Deserialize, Serialize};
use vm::{CompileSourceFileOptions, SourceFlavor};

use crate::analyzer::{LintReport, lint_source_with_flavor, lint_source_with_flavor_at_path};
use crate::completions::{CompletionCatalog, build_completion_catalog};

#[derive(Serialize)]
struct LintResponse {
    diagnostics: Vec<LintDiagnostic>,
}

#[derive(Serialize)]
struct LintDiagnostic {
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

#[derive(Deserialize)]
struct ModuleOverrideInput {
    path: String,
    source: String,
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

fn report_to_json(report: LintReport) -> Vec<u8> {
    let response = LintResponse {
        diagnostics: report
            .diagnostics
            .into_iter()
            .map(|item| LintDiagnostic {
                line: item.line,
                message: item.message,
                span: item.span.map(|span| LintSpanJson {
                    start_line: span.start_line,
                    start_col: span.start_col,
                    end_line: span.end_line,
                    end_col: span.end_col,
                }),
                rendered: item.rendered,
            })
            .collect(),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec())
}

fn completion_catalog_to_json(catalog: CompletionCatalog) -> Vec<u8> {
    serde_json::to_vec(&catalog).unwrap_or_else(|_| {
        b"{\"rustscript\":[],\"javascript\":[],\"lua\":[],\"scheme\":[]}".to_vec()
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
    let source_bytes = unpack_input(source_ptr, source_len);
    let source = match std::str::from_utf8(source_bytes) {
        Ok(value) => value,
        Err(err) => {
            let report = LintResponse {
                diagnostics: vec![LintDiagnostic {
                    line: 1,
                    message: format!("invalid utf-8 source: {err}"),
                    span: None,
                    rendered: format!("invalid utf-8 source: {err}"),
                }],
            };
            let fallback =
                serde_json::to_vec(&report).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec());
            return leak_bytes(fallback);
        }
    };

    let flavor_raw = std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)).unwrap_or("rss");
    let flavor = parse_flavor(flavor_raw);
    let report = lint_source_with_flavor(source, flavor);
    leak_bytes(report_to_json(report))
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
    let source_bytes = unpack_input(source_ptr, source_len);
    let source = match std::str::from_utf8(source_bytes) {
        Ok(value) => value,
        Err(err) => return leak_bytes(invalid_utf8_lint_response(err)),
    };

    let flavor_raw = std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)).unwrap_or("rss");
    let flavor = parse_flavor(flavor_raw);
    let path_raw = std::str::from_utf8(unpack_input(path_ptr, path_len)).unwrap_or("");
    let overrides_raw =
        std::str::from_utf8(unpack_input(overrides_ptr, overrides_len)).unwrap_or("[]");
    let options = parse_module_overrides(overrides_raw);

    let report = if path_raw.trim().is_empty() {
        lint_source_with_flavor(source, flavor)
    } else {
        lint_source_with_flavor_at_path(source, Path::new(path_raw), flavor, options)
    };
    leak_bytes(report_to_json(report))
}

#[unsafe(no_mangle)]
pub extern "C" fn completion_catalog_json() -> u64 {
    leak_bytes(completion_catalog_to_json(build_completion_catalog()))
}

fn invalid_utf8_lint_response(err: std::str::Utf8Error) -> Vec<u8> {
    let report = LintResponse {
        diagnostics: vec![LintDiagnostic {
            line: 1,
            message: format!("invalid utf-8 source: {err}"),
            span: None,
            rendered: format!("invalid utf-8 source: {err}"),
        }],
    };
    serde_json::to_vec(&report).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec())
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::completions::build_completion_catalog;

    use super::parse_flavor;
    use crate::analyzer::{lint_source_with_flavor, lint_source_with_flavor_at_path};
    use vm::{CompileSourceFileOptions, SourceFlavor};

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
    fn lint_reports_syntax_errors_for_all_supported_frontends() {
        let cases = [
            (SourceFlavor::RustScript, "let value = ;"),
            (SourceFlavor::JavaScript, "let value = ;"),
            (SourceFlavor::Lua, "local value = "),
            (SourceFlavor::Scheme, "(define value"),
        ];

        for (flavor, source) in cases {
            let report = lint_source_with_flavor(source, flavor);
            assert!(
                !report.diagnostics.is_empty(),
                "lint should fail for {flavor:?}"
            );
            assert!(
                !report.diagnostics[0].message.trim().is_empty(),
                "expected non-empty diagnostic message for {flavor:?}",
            );
            assert!(
                !report.diagnostics[0].rendered.trim().is_empty(),
                "expected rendered diagnostic output for {flavor:?}",
            );
            assert!(
                report.diagnostics[0].span.is_some(),
                "expected span diagnostics for {flavor:?}"
            );
        }
    }

    #[test]
    fn completion_catalog_reports_host_and_stdlib_entries() {
        let catalog = build_completion_catalog();
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "http::request::get_id"),
            "expected RustScript pd-edge host completion"
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "string::trim"),
            "expected RustScript stdlib completion"
        );
    }

    #[test]
    fn lint_reports_rustscript_move_semantics_diagnostics() {
        let source = r#"
            let value = "hello";
            let moved = value;
            value;
        "#;

        let report = lint_source_with_flavor(source, SourceFlavor::RustScript);
        assert!(
            !report.diagnostics.is_empty(),
            "expected move/borrow style diagnostics for RustScript"
        );
        assert!(
            report.diagnostics.iter().any(|diag| {
                diag.message.contains("moved")
                    || diag.rendered.contains("moved")
                    || diag.message.contains("borrow")
                    || diag.rendered.contains("borrow")
            }),
            "expected move/borrow wording in diagnostics, got: {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn lint_with_context_resolves_relative_imports_from_real_document_path() {
        let path = Path::new("workspace/examples/list_comp_test.rss");
        let source = r#"
            use super::stdlib::rss::iter::{range, map, filter};
            let values = filter(map(range(4), |value| value + 1), |value| value > 2);
            values;
        "#;
        let mut options = CompileSourceFileOptions::new();
        options.set_module_override_source(
            normalized_override_path(path, "../stdlib/rss/iter.rss"),
            include_str!("../../stdlib/rss/iter.rss"),
        );

        let report =
            lint_source_with_flavor_at_path(source, path, SourceFlavor::RustScript, options);
        assert!(
            report.diagnostics.is_empty(),
            "expected relative import lint to pass with real path context, got {:?}",
            report.diagnostics
        );
    }

    fn normalized_override_path(base_path: &Path, relative_spec: &str) -> String {
        let parent = base_path
            .parent()
            .expect("fixture path should have a parent");
        normalize_path_string(parent.join(relative_spec))
    }

    fn normalize_path_string(path: PathBuf) -> String {
        let raw = path.to_string_lossy().replace('\\', "/");
        let (prefix, remainder) = if raw.len() >= 2 && raw.as_bytes()[1] == b':' {
            (&raw[..2], &raw[2..])
        } else {
            ("", raw.as_str())
        };
        let absolute = remainder.starts_with('/');
        let mut segments = Vec::<&str>::new();
        for segment in remainder.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                match segments.last().copied() {
                    Some(existing) if existing != ".." => {
                        segments.pop();
                    }
                    _ if !absolute => segments.push(".."),
                    _ => {}
                }
                continue;
            }
            segments.push(segment);
        }
        let mut out = String::new();
        out.push_str(prefix);
        if absolute {
            out.push('/');
        }
        out.push_str(&segments.join("/"));
        out
    }
}
