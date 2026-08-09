use super::source_map::{SourceMap, Span};
use super::{CompileError, ParseError, SourceError, SourcePathError};

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
    let source_name = err.source_name();

    // Resolve the owning source id: by name when the error names its source,
    // or as the single-file fallback used by inline/REPL compiles (a map with
    // exactly one file at id 0). A named error is never rendered against
    // another file: when its source is missing from the map it renders as a
    // plain path/line message instead of misattributing the span.
    let source_id = match source_name {
        Some(name) => source_map.source_id_by_name(name),
        None if source_map.file(0).is_some() && source_map.file(1).is_none() => Some(0),
        None => None,
    };

    if let Some(line) = err.line()
        && let Some(source_id) = source_id
        && let Some(span) = source_map.line_span(source_id, line)
        && let Some(rendered) = render_span_snippet(source_map, span, &message)
    {
        return format!("compile error: {}", rendered.trim_end());
    }

    if let Some(line) = err.line() {
        if let Some(source_name) = source_name {
            return format!("compile error: {source_name}:{line}: {message}");
        }
        return format!("compile error: line {line}: {message}");
    }

    format!("compile error: {message}")
}

/// Render a source error (parse or compile) against the compilation-wide
/// source map carried by a [`SourcePathError`] when present, falling back to
/// a map-less render otherwise. Parse errors whose span references a source
/// id outside the map keep their path-prefixed message.
pub fn render_source_path_error(
    source_path: &std::path::Path,
    err: &SourcePathError,
    _styled: bool,
) -> String {
    match err {
        SourcePathError::SourceWithMap { error, sources } => match error {
            SourceError::Parse(parse) => render_source_error(sources, parse, _styled),
            SourceError::Compile(compile) => render_compile_error(sources, compile, _styled),
        },
        SourcePathError::Source(error) => match error {
            SourceError::Parse(parse) => {
                let render_path = parse
                    .message
                    .split_once(": ")
                    .map(|(path, _)| std::path::Path::new(path))
                    .filter(|path| path.exists())
                    .unwrap_or(source_path);
                let source = std::fs::read_to_string(render_path).unwrap_or_default();
                let mut source_map = SourceMap::new();
                let source_id = source_map.add_source(render_path.display().to_string(), source);
                let parse = parse
                    .clone()
                    .with_line_span_from_source(&source_map, source_id);
                render_source_error(&source_map, &parse, _styled)
            }
            SourceError::Compile(compile) => {
                let render_path = compile
                    .source_name()
                    .map(std::path::Path::new)
                    .filter(|path| path.exists())
                    .unwrap_or(source_path);
                let source = std::fs::read_to_string(render_path).unwrap_or_default();
                let mut source_map = SourceMap::new();
                source_map.add_source(render_path.display().to_string(), source);
                render_compile_error(&source_map, compile, _styled)
            }
        },
        SourcePathError::InvalidImportSyntax {
            path,
            line,
            message,
        } => {
            let source = std::fs::read_to_string(path).unwrap_or_default();
            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source(path.display().to_string(), source);
            let parse = ParseError::at_line(*line, message.clone())
                .with_line_span_from_source(&source_map, source_id);
            render_source_error(&source_map, &parse, _styled)
        }
        _ => err.to_string(),
    }
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
