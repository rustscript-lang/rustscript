use vm::{
    SourceError, SourceFlavor, SourceMap, SourcePathError, compile_source_with_flavor_and_options,
    render_source_error,
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
    match compile_source_with_flavor_and_options(source, flavor, embedded_stdlib_compile_options())
    {
        Ok(_) => LintReport::ok(),
        Err(SourcePathError::Source(SourceError::Parse(err))) => {
            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source("<lint>", source.to_string());
            let err = err.with_line_span_from_source(&source_map, source_id);
            let span = err.span.and_then(|span| {
                let (start_line, start_col) = source_map.line_col_for_offset(source_id, span.lo)?;
                let end_offset = if span.hi > span.lo { span.hi } else { span.lo };
                let (end_line, end_col) = source_map.line_col_for_offset(source_id, end_offset)?;
                Some(LintSpan {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                })
            });
            let rendered = render_source_error(&source_map, &err, true);
            LintReport {
                diagnostics: vec![LintDiagnostic {
                    line: err.line,
                    message: err.message,
                    span,
                    rendered,
                }],
            }
        }
        Err(SourcePathError::Source(SourceError::Compile(err))) => LintReport {
            diagnostics: vec![LintDiagnostic {
                line: 0,
                message: format!("compile error: {err:?}"),
                span: None,
                rendered: format!("compile error: {err:?}"),
            }],
        },
        Err(SourcePathError::InvalidImportSyntax { line, message, .. }) => LintReport {
            diagnostics: vec![LintDiagnostic {
                line,
                message: message.clone(),
                span: None,
                rendered: message,
            }],
        },
        Err(err) => LintReport {
            diagnostics: vec![LintDiagnostic {
                line: 0,
                message: err.to_string(),
                span: None,
                rendered: err.to_string(),
            }],
        },
    }
}
