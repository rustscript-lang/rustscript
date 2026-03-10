use std::path::Path;

use vm::{
    CompileSourceFileOptions, CompiledProgram, SourceError, SourceFlavor, SourceMap, SourcePathError,
    lint_unknown_inferred_local_types_at_path_with_options,
    lint_unknown_inferred_local_types_with_options,
    compile_source_at_path_with_flavor_and_options, compile_source_with_flavor_and_options,
    lint_trailing_function_return_semicolons, render_compile_error, render_source_error,
};

use crate::stdlib::embedded_stdlib_compile_options;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LintSeverity {
    Error,
    Warning,
}

impl LintSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

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
    pub severity: LintSeverity,
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
    let options = embedded_stdlib_compile_options();
    lint_compile_result(
        source,
        flavor,
        None,
        &options,
        compile_source_with_flavor_and_options(source, flavor, options.clone()),
    )
}

pub fn lint_source_with_flavor_at_path(
    source: &str,
    path: &Path,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> LintReport {
    let path = path.to_path_buf();
    lint_compile_result(
        source,
        flavor,
        Some(path.as_path()),
        &options,
        compile_source_at_path_with_flavor_and_options(&path, source, flavor, options.clone()),
    )
}

fn lint_compile_result(
    source: &str,
    flavor: SourceFlavor,
    path: Option<&Path>,
    options: &CompileSourceFileOptions,
    result: Result<vm::CompiledProgram, SourcePathError>,
) -> LintReport {
    match result {
        Ok(compiled) => {
            let diagnostics = lint_success_diagnostics(source, flavor, &compiled, path, options);
            if diagnostics.is_empty() {
                LintReport::ok()
            } else {
                LintReport { diagnostics }
            }
        }
        Err(SourcePathError::Source(SourceError::Parse(err))) => {
            let mut diagnostics = lint_trailing_function_return_semicolon_diagnostics(source, flavor);
            diagnostics.push(lint_diagnostic_from_parse_error(
                source,
                err,
                LintSeverity::Error,
            ));
            LintReport { diagnostics }
        }
        Err(SourcePathError::Source(SourceError::Compile(err))) => {
            let mut diagnostics = lint_trailing_function_return_semicolon_diagnostics(source, flavor);
            diagnostics.extend(lint_unknown_inferred_local_diagnostics(
                source, flavor, path, options,
            ));
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
                severity: LintSeverity::Error,
                message: err.diagnostic_message(),
                span,
                rendered,
            });
            LintReport { diagnostics }
        }
        Err(SourcePathError::InvalidImportSyntax { line, message, .. }) => {
            let mut diagnostics = lint_trailing_function_return_semicolon_diagnostics(source, flavor);
            diagnostics.push(LintDiagnostic {
                line,
                severity: LintSeverity::Error,
                message: message.clone(),
                span: None,
                rendered: message,
            });
            LintReport { diagnostics }
        }
        Err(err) => {
            let mut diagnostics = lint_trailing_function_return_semicolon_diagnostics(source, flavor);
            diagnostics.push(LintDiagnostic {
                line: 0,
                severity: LintSeverity::Error,
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
        .map(|err| lint_diagnostic_from_parse_error(source, err, LintSeverity::Warning))
        .collect()
}

pub(crate) fn lint_success_diagnostics(
    source: &str,
    flavor: SourceFlavor,
    compiled: &CompiledProgram,
    path: Option<&Path>,
    options: &CompileSourceFileOptions,
) -> Vec<LintDiagnostic> {
    let mut diagnostics = lint_trailing_function_return_semicolon_diagnostics(source, flavor);
    let _ = compiled;
    diagnostics.extend(lint_unknown_inferred_local_diagnostics(
        source, flavor, path, options,
    ));
    diagnostics
}

fn lint_diagnostic_from_parse_error(
    source: &str,
    err: vm::ParseError,
    severity: LintSeverity,
) -> LintDiagnostic {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<lint>", source.to_string());
    let err = err.with_line_span_from_source(&source_map, source_id);
    let span = err
        .span
        .and_then(|span| lint_span_from_source_span(&source_map, source_id, span.lo, span.hi));
    let rendered = render_source_error(&source_map, &err, true);
    LintDiagnostic {
        line: err.line,
        severity,
        message: err.message,
        span,
        rendered,
    }
}

fn lint_unknown_inferred_local_diagnostics(
    source: &str,
    flavor: SourceFlavor,
    path: Option<&Path>,
    options: &CompileSourceFileOptions,
) -> Vec<LintDiagnostic> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<lint>", source.to_string());
    let warnings = if let Some(path) = path {
        lint_unknown_inferred_local_types_at_path_with_options(
            path,
            source,
            flavor,
            options.clone(),
        )
    } else {
        lint_unknown_inferred_local_types_with_options(source, flavor, options.clone())
    };
    let Ok(warnings) = warnings else {
        return Vec::new();
    };

    warnings
        .into_iter()
        .filter_map(|warning| {
            let span = warning
                .span
                .or_else(|| source_map.line_span(source_id, warning.line))?;
            let lint_span = lint_span_from_source_span(&source_map, source_id, span.lo, span.hi);
            let message = format!(
                "compiler could not determine the type of local '{}'",
                warning.name
            );
            let rendered = render_lint_warning(&source_map, source_id, span.lo, span.hi, &message);
            Some(LintDiagnostic {
                line: warning.line,
                severity: LintSeverity::Warning,
                message,
                span: lint_span,
                rendered,
            })
        })
        .collect()
}

fn render_lint_warning(
    source_map: &SourceMap,
    source_id: u32,
    lo: usize,
    hi: usize,
    message: &str,
) -> String {
    let Some(file) = source_map.file(source_id) else {
        return format!("warning: {message}");
    };
    let Some((line, col)) = file.line_col_for_offset(lo) else {
        return format!("warning: {message}");
    };
    let Some(line_text) = file.line_text(line) else {
        return format!("warning: {}:{line}:{col}: {message}", file.name);
    };
    let pointer_width = hi.saturating_sub(lo).max(1);
    let pointer = format!(
        "{}{}",
        " ".repeat(col.saturating_sub(1)),
        "^".repeat(pointer_width)
    );
    format!(
        "warning: {message}\n --> {}:{line}:{col}\n  |\n{line:>3} | {line_text}\n  | {pointer}",
        file.name
    )
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
