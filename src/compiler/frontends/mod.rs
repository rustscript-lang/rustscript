mod rustscript;

use std::collections::HashMap;

use crate::compiler::source_map::{LoweredSource, SourceMap};

use super::{
    CompileSourceFileOptions, ParseError, ReplLocalBinding, SharedParserOptions, SourceFlavor,
    ir::FrontendIr,
    parser::{Parser, ParserDialect},
};

// REPL snippets carry the persisted binding table alongside the parsed IR.
pub(super) struct ParsedRustScriptReplSource {
    pub ir: FrontendIr,
    pub bindings: Vec<ReplLocalBinding>,
}

pub(super) fn parse_source(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<FrontendIr, ParseError> {
    match flavor {
        SourceFlavor::RustScript => {
            let lowered = rustscript::lower(source)?;
            parse_lowered_with_mapping(source, lowered, false, false, true)
        }
        SourceFlavor::JavaScript | SourceFlavor::Lua => {
            let Some(plugin) = options.source_plugin_for_flavor(flavor) else {
                return Err(ParseError::new(format!(
                    "no frontend plugin registered for {flavor:?} source"
                )));
            };
            plugin.parse_source(source)
        }
    }
}

pub(crate) fn parser_dialect_for_flavor(
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Option<&'static dyn ParserDialect> {
    match flavor {
        SourceFlavor::RustScript => Some(rustscript::parser_dialect()),
        SourceFlavor::JavaScript | SourceFlavor::Lua => options
            .source_plugin_for_flavor(flavor)
            .and_then(|plugin| plugin.parser_dialect()),
    }
}

pub fn parse_source_with_dialect(
    source: &str,
    dialect: &'static dyn ParserDialect,
    options: SharedParserOptions,
) -> Result<FrontendIr, ParseError> {
    parse_with_parser(
        source,
        options.source_id,
        options.allow_implicit_externs,
        options.allow_implicit_semicolons,
        options.enforce_mutable_bindings,
        dialect,
    )
}

pub(super) fn parse_rustscript_repl_source(
    source: &str,
    predefined_locals: &[ReplLocalBinding],
) -> Result<ParsedRustScriptReplSource, ParseError> {
    let lowered = rustscript::lower(source)?;
    parse_lowered_repl_with_mapping(source, lowered, predefined_locals, false, false, true)
}

pub fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

pub fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn parse_with_parser(
    source: &str,
    source_id: u32,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    enforce_mutable_bindings: bool,
    dialect: &'static dyn ParserDialect,
) -> Result<FrontendIr, ParseError> {
    let mut parser = Parser::new(
        source,
        source_id,
        allow_implicit_externs,
        allow_implicit_semicolons,
        enforce_mutable_bindings,
        dialect,
    )?;
    let stmts = parser.parse_program()?;
    Ok(FrontendIr {
        stmts,
        locals: parser.local_count(),
        local_bindings: parser.local_bindings(),
        struct_schemas: parser.struct_schemas(),
        unknown_type_spans: parser.unknown_type_spans(),
        functions: parser.function_decls(),
        function_impls: parser.function_impls(),
        stmt_sources: Vec::new(),
        function_sources: HashMap::new(),
    })
}

fn parse_repl_with_parser(
    source: &str,
    source_id: u32,
    predefined_locals: &[ReplLocalBinding],
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    enforce_mutable_bindings: bool,
    dialect: &'static dyn ParserDialect,
) -> Result<ParsedRustScriptReplSource, ParseError> {
    let mut parser = Parser::new_with_predeclared_locals(
        source,
        source_id,
        allow_implicit_externs,
        allow_implicit_semicolons,
        enforce_mutable_bindings,
        dialect,
        predefined_locals,
    )?;
    let stmts = parser.parse_program()?;
    let bindings = parser.local_bindings_with_mutability();

    Ok(ParsedRustScriptReplSource {
        ir: FrontendIr {
            stmts,
            locals: parser.local_count(),
            local_bindings: parser.local_bindings(),
            struct_schemas: parser.struct_schemas(),
            unknown_type_spans: parser.unknown_type_spans(),
            functions: parser.function_decls(),
            function_impls: parser.function_impls(),
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
        },
        bindings,
    })
}

fn parse_lowered_with_mapping(
    original_source: &str,
    lowered: LoweredSource,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    enforce_mutable_bindings: bool,
) -> Result<FrontendIr, ParseError> {
    let mut source_map = SourceMap::new();
    let original_source_id = source_map.add_source("<source>", original_source.to_string());
    let lowered_source_id = source_map.add_source("<lowered>", lowered.text.clone());

    match parse_with_parser(
        &lowered.text,
        lowered_source_id,
        allow_implicit_externs,
        allow_implicit_semicolons,
        enforce_mutable_bindings,
        rustscript::parser_dialect(),
    ) {
        Ok(mut ir) => {
            map_spans_to_original_source(
                &mut ir.unknown_type_spans,
                &lowered,
                &source_map,
                lowered_source_id,
                original_source_id,
            );
            Ok(ir)
        }
        Err(mut err) => {
            err = err.with_line_span_from_source(&source_map, lowered_source_id);
            let mapped_span = err.span.and_then(|span| {
                lowered
                    .mapping
                    .map_span(&source_map, lowered_source_id, original_source_id, span)
            });
            if let Some(mapped) = mapped_span {
                err.span = Some(mapped);
                if let Some((line, _)) =
                    source_map.line_col_for_offset(original_source_id, mapped.lo)
                {
                    err.line = line;
                }
            } else {
                let mapped_line = lowered
                    .mapping
                    .lowered_to_original_line
                    .get(err.line.saturating_sub(1))
                    .copied()
                    .unwrap_or(err.line)
                    .max(1);
                let original_line = source_map
                    .file(original_source_id)
                    .map(|file| mapped_line.min(file.line_count().max(1)))
                    .unwrap_or(mapped_line);
                err.line = original_line;
                err.span = source_map.line_span(original_source_id, original_line);
            }
            Err(err)
        }
    }
}

fn parse_lowered_repl_with_mapping(
    original_source: &str,
    lowered: LoweredSource,
    predefined_locals: &[ReplLocalBinding],
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    enforce_mutable_bindings: bool,
) -> Result<ParsedRustScriptReplSource, ParseError> {
    let mut source_map = SourceMap::new();
    let original_source_id = source_map.add_source("<source>", original_source.to_string());
    let lowered_source_id = source_map.add_source("<lowered>", lowered.text.clone());

    match parse_repl_with_parser(
        &lowered.text,
        lowered_source_id,
        predefined_locals,
        allow_implicit_externs,
        allow_implicit_semicolons,
        enforce_mutable_bindings,
        rustscript::parser_dialect(),
    ) {
        Ok(mut parsed) => {
            map_spans_to_original_source(
                &mut parsed.ir.unknown_type_spans,
                &lowered,
                &source_map,
                lowered_source_id,
                original_source_id,
            );
            Ok(parsed)
        }
        Err(mut err) => {
            err = err.with_line_span_from_source(&source_map, lowered_source_id);
            let mapped_span = err.span.and_then(|span| {
                lowered
                    .mapping
                    .map_span(&source_map, lowered_source_id, original_source_id, span)
            });
            if let Some(mapped) = mapped_span {
                err.span = Some(mapped);
                if let Some((line, _)) =
                    source_map.line_col_for_offset(original_source_id, mapped.lo)
                {
                    err.line = line;
                }
            } else {
                let mapped_line = lowered
                    .mapping
                    .lowered_to_original_line
                    .get(err.line.saturating_sub(1))
                    .copied()
                    .unwrap_or(err.line)
                    .max(1);
                let original_line = source_map
                    .file(original_source_id)
                    .map(|file| mapped_line.min(file.line_count().max(1)))
                    .unwrap_or(mapped_line);
                err.line = original_line;
                err.span = source_map.line_span(original_source_id, original_line);
            }
            Err(err)
        }
    }
}

fn map_spans_to_original_source(
    spans: &mut [crate::compiler::source_map::Span],
    lowered: &LoweredSource,
    source_map: &SourceMap,
    lowered_source_id: u32,
    original_source_id: u32,
) {
    for span in spans {
        if let Some(mapped) =
            lowered
                .mapping
                .map_span(source_map, lowered_source_id, original_source_id, *span)
        {
            *span = mapped;
        }
    }
}
