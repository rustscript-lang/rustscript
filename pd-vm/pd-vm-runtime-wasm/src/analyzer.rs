use vm::{
    SourceError, SourceFlavor, SourceMap, SourcePathError, compile_source_with_flavor_and_options,
    lint_trailing_function_return_semicolons, render_compile_error, render_source_error,
};

use crate::stdlib::embedded_stdlib_compile_options;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintSpan {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintDiagnostic {
    pub line: usize,
    pub message: String,
    pub span: Option<LintSpan>,
    pub rendered: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintReport {
    pub diagnostics: Vec<LintDiagnostic>,
}

impl LintReport {
    pub fn ok() -> Self {
        Self {
            diagnostics: Vec::new(),
        }
    }
}

pub fn lint_source_with_flavor(source: &str, flavor: SourceFlavor) -> LintReport {
    let mut diagnostics = lint_trailing_function_return_semicolon_diagnostics(source, flavor);
    match compile_source_with_flavor_and_options(source, flavor, embedded_stdlib_compile_options())
    {
        Ok(_) => {
            if diagnostics.is_empty() {
                LintReport::ok()
            } else {
                LintReport { diagnostics }
            }
        }
        Err(SourcePathError::Source(SourceError::Parse(err))) => {
            diagnostics.push(lint_diagnostic_from_parse_error(source, err));
            LintReport { diagnostics }
        }
        Err(SourcePathError::Source(SourceError::Compile(err))) => {
            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source("<lint>", source.to_string());
            let line = err.line().unwrap_or(0);
            let span = err.line().and_then(|value| {
                let span = source_map.line_span(source_id, value)?;
                lint_span_from_source_span(&source_map, source_id, span.lo, span.hi)
            });
            let rendered = render_compile_error(&source_map, &err, true);
            diagnostics.push(LintDiagnostic {
                line,
                message: err.diagnostic_message(),
                span,
                rendered,
            });
            LintReport { diagnostics }
        }
        Err(SourcePathError::InvalidImportSyntax { line, message, .. }) => {
            diagnostics.push(LintDiagnostic {
                line,
                message: message.clone(),
                span: None,
                rendered: message,
            });
            LintReport { diagnostics }
        }
        Err(err) => {
            diagnostics.push(LintDiagnostic {
                line: 0,
                message: err.to_string(),
                span: None,
                rendered: err.to_string(),
            });
            LintReport { diagnostics }
        }
    }
}

fn lint_trailing_function_return_semicolon_diagnostics(
    source: &str,
    flavor: SourceFlavor,
) -> Vec<LintDiagnostic> {
    let Ok(errors) = lint_trailing_function_return_semicolons(source, flavor) else {
        return Vec::new();
    };
    errors
        .into_iter()
        .map(|err| lint_diagnostic_from_parse_error(source, err))
        .collect()
}

fn lint_diagnostic_from_parse_error(source: &str, err: vm::ParseError) -> LintDiagnostic {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<lint>", source.to_string());
    let err = err.with_line_span_from_source(&source_map, source_id);
    let span = err
        .span
        .and_then(|span| lint_span_from_source_span(&source_map, source_id, span.lo, span.hi));
    let rendered = render_source_error(&source_map, &err, true);
    LintDiagnostic {
        line: err.line,
        message: err.message,
        span,
        rendered,
    }
}

fn lint_span_from_source_span(
    source_map: &SourceMap,
    source_id: u32,
    lo: usize,
    hi: usize,
) -> Option<LintSpan> {
    let (start_line, start_col) = source_map.line_col_for_offset(source_id, lo)?;
    let end_offset = if hi > lo { hi } else { lo };
    let (end_line, end_col) = source_map.line_col_for_offset(source_id, end_offset)?;
    Some(LintSpan {
        start_line,
        start_col,
        end_line,
        end_col,
    })
}
