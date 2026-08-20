mod rustscript;

use std::collections::HashMap;
use std::sync::Arc;

use crate::compiler::source_map::{LoweredSource, SourceMap};
use crate::host_api::HostApiCatalog;

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
    parse_source_with_source_id(source, flavor, options, 0)
}

/// Parse `source` and attribute every produced span to `original_source_id`.
///
/// The id belongs to the compilation-wide [`SourceMap`] built by the source
/// loader, whose ids are the semantic module graph's
/// [`SourceId`](crate::compiler::modules::SourceId) space. Spans produced by
/// this parse (including the error span on failure) therefore stay owned by
/// the module's source after unit merge. The default id `0` preserves the
/// legacy single-source behavior for entry points that build their own map.
pub(super) fn parse_source_with_source_id(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    original_source_id: u32,
) -> Result<FrontendIr, ParseError> {
    parse_source_with_source_id_and_externs(source, flavor, options, original_source_id, false)
}

/// Parse one module's source for the source loader (module mode).
///
/// Module-mode parses enable the parser's implicit-extern fallback so that
/// calls to imported module functions and module namespace members parse
/// before the loader resolves them by [`SymbolId`](crate::compiler::modules::SymbolId).
/// The produced IR carries the implicit-extern names on
/// [`FrontendIr::implicit_extern_names`] so the loader keeps those synthetic
/// declarations out of module declaration/export tables.
pub(super) fn parse_module_source_with_source_id(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    original_source_id: u32,
) -> Result<FrontendIr, ParseError> {
    parse_source_with_source_id_and_externs(source, flavor, options, original_source_id, true)
}

fn parse_source_with_source_id_and_externs(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    original_source_id: u32,
    allow_implicit_externs: bool,
) -> Result<FrontendIr, ParseError> {
    match flavor {
        SourceFlavor::RustScript => {
            let lowered = rustscript::lower(source)?;
            parse_lowered_with_mapping(
                source,
                lowered,
                allow_implicit_externs,
                false,
                true,
                original_source_id,
                options.host_api_catalog().cloned(),
            )
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
        options.import_scan_mode,
        dialect,
        None,
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
    import_scan_mode: bool,
    dialect: &'static dyn ParserDialect,
    host_catalog: Option<Arc<HostApiCatalog>>,
) -> Result<FrontendIr, ParseError> {
    let mut parser = match host_catalog {
        Some(catalog) => Parser::new_with_host_catalog(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            import_scan_mode,
            dialect,
            catalog,
        )?,
        None => Parser::new(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            import_scan_mode,
            dialect,
        )?,
    };
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
        use_declarations: parser.use_declarations(),
        implicit_extern_names: parser.implicit_extern_names(),
        host_api_metadata: parser.host_api_metadata(),
        semantic_index: None,
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
            use_declarations: parser.use_declarations(),
            implicit_extern_names: parser.implicit_extern_names(),
            host_api_metadata: None,
            semantic_index: None,
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
    original_source_id: u32,
    host_catalog: Option<Arc<HostApiCatalog>>,
) -> Result<FrontendIr, ParseError> {
    let mut source_map = SourceMap::new();
    source_map.add_source_at(original_source_id, "<source>", original_source.to_string());
    let lowered_source_id = source_map.add_source("<lowered>", lowered.text.clone());

    match parse_with_parser(
        &lowered.text,
        lowered_source_id,
        allow_implicit_externs,
        allow_implicit_semicolons,
        enforce_mutable_bindings,
        false,
        rustscript::parser_dialect(),
        host_catalog,
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

#[cfg(test)]
mod host_catalog_frontend_tests {
    use std::sync::Arc;

    use crate::compiler::CompileSourceFileOptions;
    use crate::host_api::{
        HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamSchema, HostTypeSchema,
    };

    use super::{SourceFlavor, parse_source};

    fn read_catalog() -> Arc<HostApiCatalog> {
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::new(
            "acme::read",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
        ));
        Arc::new(builder.build().expect("test catalog must be valid"))
    }

    #[test]
    fn empty_source_with_catalog_yields_some_matching_fingerprint_and_zero_indices() {
        let catalog = Arc::new(HostApiCatalog::builder().build().unwrap());
        let options =
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
        let ir = parse_source("", SourceFlavor::RustScript, &options).expect("parse succeeds");
        let metadata = ir.host_api_metadata.as_ref().expect("metadata present");
        assert_eq!(metadata.fingerprint(), catalog.fingerprint());
        assert_eq!(metadata.function_indices().len(), 0);
    }

    #[test]
    fn no_catalog_yields_none() {
        let ir = parse_source(
            "use acme; acme::read(\"x\");\n",
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("parse succeeds");
        assert!(
            ir.host_api_metadata.is_none(),
            "no catalog means no metadata"
        );
    }

    #[test]
    fn host_call_records_complete_candidate_at_its_index() {
        let catalog = read_catalog();
        let options =
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
        let ir = parse_source(
            "use acme; acme::read(\"x\");\n",
            SourceFlavor::RustScript,
            &options,
        )
        .expect("host call parse succeeds");
        let metadata = ir.host_api_metadata.as_ref().expect("metadata present");
        assert_eq!(metadata.fingerprint(), catalog.fingerprint());
        let read_decl = ir
            .functions
            .iter()
            .find(|decl| decl.name == "acme::read")
            .expect("host read decl present");
        let candidates = metadata
            .candidates(read_decl.index)
            .expect("candidates recorded");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "acme::read");
        assert_eq!(candidates[0].params.len(), 1);
        // Candidate-level: no schema preselection on the flat decl (arg
        // schemas stay unresolved `None`, no return schema).
        assert_eq!(
            read_decl.arg_schemas,
            vec![None],
            "no candidate arg schema preselection"
        );
        assert_eq!(read_decl.return_type, crate::ValueType::Unknown);
        assert!(read_decl.return_schema.is_none());
    }

    #[test]
    fn distinct_modules_with_same_options_share_fingerprint() {
        let catalog = read_catalog();
        let options =
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
        let with_call = parse_source(
            "use acme; acme::read(\"a\");\n",
            SourceFlavor::RustScript,
            &options,
        )
        .expect("parse succeeds");
        let without_call = parse_source("let x = 1; x + 1;\n", SourceFlavor::RustScript, &options)
            .expect("parse succeeds");
        let fp1 = with_call
            .host_api_metadata
            .as_ref()
            .expect("some")
            .fingerprint();
        let fp2 = without_call
            .host_api_metadata
            .as_ref()
            .expect("some")
            .fingerprint();
        assert_eq!(fp1, fp2, "same options snapshot must yield one fingerprint");
        assert_eq!(fp1, catalog.fingerprint());
    }
}
