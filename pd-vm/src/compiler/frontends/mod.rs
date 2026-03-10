mod javascript;
mod lua;
mod rustscript;
mod scheme;

use std::collections::HashMap;

use crate::compiler::source_map::{LoweredSource, SourceMap};

use super::{
    ParseError, ReplLocalBinding, SourceFlavor,
    ir::{FrontendIr, LocalSlot},
    parser::{Parser, ParserDialect},
};

pub(crate) use scheme::SchemeImportContext;

trait FrontendCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError>;
}

struct RustScriptCompiler;
struct JavaScriptCompiler;
struct LuaCompiler;
struct SchemeCompiler;

// REPL snippets need a small amount of extra state that normal source compilation
// does not carry: the persisted binding table and which slots are live on entry.
pub(super) struct ParsedRustScriptReplSource {
    pub ir: FrontendIr,
    pub bindings: Vec<ReplLocalBinding>,
    pub entry_definite_locals: Vec<LocalSlot>,
}

pub(super) fn parse_source(source: &str, flavor: SourceFlavor) -> Result<FrontendIr, ParseError> {
    let frontend: &dyn FrontendCompiler = match flavor {
        SourceFlavor::RustScript => &RustScriptCompiler,
        SourceFlavor::JavaScript => &JavaScriptCompiler,
        SourceFlavor::Lua => &LuaCompiler,
        SourceFlavor::Scheme => &SchemeCompiler,
    };
    frontend.lower_to_ir(source)
}

pub(crate) fn parser_dialect_for_flavor(
    flavor: SourceFlavor,
) -> Option<&'static dyn ParserDialect> {
    match flavor {
        SourceFlavor::RustScript => Some(rustscript::parser_dialect()),
        SourceFlavor::JavaScript => Some(javascript::parser_dialect()),
        SourceFlavor::Lua | SourceFlavor::Scheme => None,
    }
}

pub(super) fn parse_rustscript_repl_source(
    source: &str,
    predefined_locals: &[ReplLocalBinding],
) -> Result<ParsedRustScriptReplSource, ParseError> {
    let lowered = rustscript::lower(source)?;
    parse_lowered_repl_with_mapping(source, lowered, predefined_locals, false, false, true)
}

pub(super) fn parse_source_with_scheme_import_context(
    source: &str,
    flavor: SourceFlavor,
    scheme_import_context: Option<&SchemeImportContext>,
) -> Result<FrontendIr, ParseError> {
    match flavor {
        SourceFlavor::Scheme => {
            scheme::lower_to_ir_with_import_context(source, scheme_import_context)
        }
        SourceFlavor::RustScript | SourceFlavor::JavaScript | SourceFlavor::Lua => {
            parse_source(source, flavor)
        }
    }
}

pub(super) fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

pub(super) fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

impl FrontendCompiler for RustScriptCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        let lowered = rustscript::lower(source)?;
        parse_lowered_with_mapping(source, lowered, false, false, true)
    }
}

impl FrontendCompiler for JavaScriptCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        javascript::lower_to_ir(source)
    }
}

impl FrontendCompiler for LuaCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        lua::lower_to_ir(source)
    }
}

impl FrontendCompiler for SchemeCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        scheme::lower_to_ir(source)
    }
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
    // These slots are carried in from earlier REPL entries, so availability starts
    // with them marked as definitely present.
    let predeclared = parser
        .local_bindings()
        .into_iter()
        .filter_map(|(name, index)| {
            predefined_locals
                .iter()
                .any(|binding| binding.name == name)
                .then_some(index)
        })
        .collect::<Vec<_>>();
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
        entry_definite_locals: predeclared,
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
        Ok(mut result) => {
            map_spans_to_original_source(
                &mut result.ir.unknown_type_spans,
                &lowered,
                &source_map,
                lowered_source_id,
                original_source_id,
            );
            Ok(result)
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
    spans: &mut Vec<crate::compiler::source_map::Span>,
    lowered: &LoweredSource,
    source_map: &SourceMap,
    lowered_source_id: u32,
    original_source_id: u32,
) {
    for span in spans.iter_mut() {
        if let Some(mapped) =
            lowered
                .mapping
                .map_span(source_map, lowered_source_id, original_source_id, *span)
        {
            *span = mapped;
        }
    }
}
