use super::source_map::{SourceMap, Span};
use super::{CompileError, ParseError};

pub fn render_source_error(source_map: &SourceMap, err: &ParseError, _styled: bool) -> String {
    let code_prefix = err
        .code
        .as_deref()
        .map(|code| format!("error[{code}]"))
        .unwrap_or_else(|| "error".to_string());

    if let Some(span) = err.span
        && let Some(rendered) = render_span_snippet(source_map, span, &err.message)
    {
        return format!("{code_prefix}: {}", rendered.trim_end());
    }

    format!("{code_prefix}: line {}: {}", err.line, err.message)
}

pub fn render_compile_error(source_map: &SourceMap, err: &CompileError, _styled: bool) -> String {
    let message = err.diagnostic_message();
    let source_id = err
        .source_name()
        .and_then(|name| source_map.source_id_by_name(name))
        .unwrap_or(0);

    if let Some(line) = err.line()
        && let Some(span) = source_map.line_span(source_id, line)
        && let Some(rendered) = render_span_snippet(source_map, span, &message)
    {
        return format!("compile error: {}", rendered.trim_end());
    }

    if let Some(line) = err.line() {
        if let Some(source_name) = err.source_name() {
            return format!("compile error: {source_name}:{line}: {message}");
        }
        return format!("compile error: line {line}: {message}");
    }

    format!("compile error: {message}")
}

fn render_span_snippet(source_map: &SourceMap, span: Span, message: &str) -> Option<String> {
    let file = source_map.file(span.source_id)?;
    let (line, col) = file.line_col_for_offset(span.lo)?;
    let line_text = file.line_text(line)?;
    let pointer_width = span.len().max(1);
    let pointer = format!(
        "{}{}",
        " ".repeat(col.saturating_sub(1)),
        "^".repeat(pointer_width)
    );
    Some(format!(
        "{message}\n --> {}:{line}:{col}\n  |\n{line:>3} | {line_text}\n  | {pointer}",
        file.name
    ))
}
