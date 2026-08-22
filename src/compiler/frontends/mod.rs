mod rustscript;

use std::collections::HashMap;
use std::sync::Arc;

use crate::compiler::source_map::{LoweredSource, SourceMap, Span};
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
    parse_rustscript_repl_source_with_catalog(source, predefined_locals, None)
}

/// REPL parse with an optional catalog snapshot: when `Some`, the parsed IR
/// carries `host_api_metadata` so standard host calls compile to exact V13
/// `HostImport` schemas (never a name-only fallback).
pub(super) fn parse_rustscript_repl_source_with_catalog(
    source: &str,
    predefined_locals: &[ReplLocalBinding],
    host_catalog: Option<Arc<HostApiCatalog>>,
) -> Result<ParsedRustScriptReplSource, ParseError> {
    let lowered = rustscript::lower(source)?;
    parse_lowered_repl_with_mapping(
        source,
        lowered,
        predefined_locals,
        false,
        false,
        true,
        host_catalog,
    )
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
        parsed_semantic_index: Some(parser.take_parsed_semantic_index()),
        catalog_visibility: Some(parser.take_catalog_visibility()),
        lexer_tokens: parser.take_lexer_tokens(),
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
    host_catalog: Option<Arc<HostApiCatalog>>,
) -> Result<ParsedRustScriptReplSource, ParseError> {
    let mut parser = match host_catalog {
        Some(catalog) => Parser::new_with_predeclared_locals_and_host_catalog(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            dialect,
            predefined_locals,
            Some(catalog),
        )?,
        None => Parser::new_with_predeclared_locals(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            dialect,
            predefined_locals,
        )?,
    };
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
            host_api_metadata: parser.host_api_metadata(),
            semantic_index: None,
            parsed_semantic_index: Some(parser.take_parsed_semantic_index()),
            catalog_visibility: Some(parser.take_catalog_visibility()),
            lexer_tokens: parser.take_lexer_tokens(),
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
            remap_lowered_spans(
                ir.parsed_semantic_index.as_mut(),
                &mut ir.unknown_type_spans,
                &mut ir.lexer_tokens,
                &lowered,
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
    host_catalog: Option<Arc<HostApiCatalog>>,
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
        host_catalog,
    ) {
        Ok(mut parsed) => {
            remap_lowered_spans(
                parsed.ir.parsed_semantic_index.as_mut(),
                &mut parsed.ir.unknown_type_spans,
                &mut parsed.ir.lexer_tokens,
                &lowered,
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

/// Remap every parser-produced span from the lowered text back to the
/// original source using the exact byte mapping recorded during lowering.
///
/// Both the parsed semantic index (call sites, local decls/refs, function
/// decls/refs, lexical scopes) and the unknown-type spans are remapped so
/// every span slices the original source exactly. The mapping comes from
/// `lowered.byte_mapping`, which is generated during lowering — never from
/// searching the source text afterwards.
fn remap_lowered_spans(
    parsed_index: Option<&mut crate::compiler::ir::ParsedSemanticIndex>,
    unknown_type_spans: &mut [Span],
    lexer_tokens: &mut [crate::compiler::ir::LexerToken],
    lowered: &LoweredSource,
    lowered_source_id: u32,
    original_source_id: u32,
) {
    let map = |span: &mut Span| {
        if let Some(mapped) =
            lowered
                .byte_mapping
                .map_span(original_source_id, *span, lowered_source_id)
        {
            *span = mapped;
        }
    };

    if let Some(index) = parsed_index {
        for site in &mut index.call_sites {
            map(&mut site.callee_span);
            map(&mut site.expr_span);
        }
        for decl in &mut index.local_decls {
            map(&mut decl.ident_span);
            map(&mut decl.stmt_span);
        }
        for reference in &mut index.local_refs {
            map(&mut reference.ident_span);
        }
        for decl in &mut index.func_decls {
            map(&mut decl.ident_span);
        }
        for reference in &mut index.func_refs {
            map(&mut reference.ident_span);
        }
        for scope in &mut index.scopes {
            map(&mut scope.range);
        }
        for site in &mut index.stmt_spans {
            map(&mut site.span);
        }
    }
    for span in unknown_type_spans {
        map(span);
    }
    for token in lexer_tokens {
        map(&mut token.span);
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

#[cfg(test)]
mod ordinary_call_provenance_tests {
    use crate::compiler::CompileSourceFileOptions;
    use crate::compiler::ir::{Expr, Stmt};
    use crate::compiler::source_map::Span;

    use super::{SourceFlavor, parse_source};

    fn parse(source: &str) -> crate::compiler::ir::FrontendIr {
        parse_source(
            source,
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("source must parse")
    }

    fn stmt_call_exprs(ir: &crate::compiler::ir::FrontendIr) -> Vec<&Expr> {
        ir.stmts
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                    Some(expr)
                }
                _ => None,
            })
            .collect()
    }

    /// RustScript lowering is the identity, so span `.lo`/`.hi` are byte
    /// offsets into the original source string.
    fn span_slice(source: &str, span: Span) -> String {
        source
            .get(span.lo..span.hi)
            .expect("span must slice source")
            .to_string()
    }

    /// Two direct calls on the same source line get distinct stable ids and
    /// exact callee + full-call slices.
    #[test]
    fn repeated_same_line_direct_calls_have_distinct_ids_and_exact_slices() {
        let source = "fn twice(x) { x + x }\ntwice(1); twice(2);\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");
        assert_eq!(index.call_sites.len(), 2, "two direct calls recorded");

        let exprs = stmt_call_exprs(&ir);
        assert_eq!(exprs.len(), 2);
        let Expr::Call(_, _, _, _, first_id) = exprs[0] else {
            panic!("first stmt must be an ordinary Call");
        };
        let Expr::Call(_, _, _, _, second_id) = exprs[1] else {
            panic!("second stmt must be an ordinary Call");
        };
        let first_id = first_id.expect("first call has provenance id");
        let second_id = second_id.expect("second call has provenance id");
        assert_ne!(first_id, second_id, "distinct calls must get distinct ids");

        let first_site = index
            .call_sites
            .iter()
            .find(|site| site.id == first_id)
            .expect("first call site recorded");
        let second_site = index
            .call_sites
            .iter()
            .find(|site| site.id == second_id)
            .expect("second call site recorded");

        assert_eq!(span_slice(source, first_site.callee_span), "twice");
        assert_eq!(span_slice(source, first_site.expr_span), "twice(1)");
        assert_eq!(span_slice(source, second_site.callee_span), "twice");
        assert_eq!(span_slice(source, second_site.expr_span), "twice(2)");
        assert_eq!(
            first_site.expr_span.lo, first_site.callee_span.lo,
            "expr span starts at callee start"
        );
        assert!(
            first_site.expr_span.hi < second_site.callee_span.lo,
            "first call ends before the second callee"
        );
    }

    /// Nested direct calls record exact inner and outer spans; the outer expr
    /// span covers the whole `f(g(1))` and the inner covers `g(1)`.
    #[test]
    fn nested_direct_calls_have_exact_inner_and_outer_slices() {
        let source = "fn g(x) { x }\nfn f(x) { x }\nf(g(1));\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");
        assert_eq!(index.call_sites.len(), 2, "inner and outer calls recorded");

        let mut callees: Vec<String> = index
            .call_sites
            .iter()
            .map(|site| span_slice(source, site.callee_span))
            .collect();
        callees.sort_unstable();
        assert_eq!(callees, vec!["f", "g"]);

        let inner = index
            .call_sites
            .iter()
            .find(|site| span_slice(source, site.callee_span) == "g")
            .expect("inner site");
        let outer = index
            .call_sites
            .iter()
            .find(|site| span_slice(source, site.callee_span) == "f")
            .expect("outer site");
        assert_eq!(span_slice(source, inner.expr_span), "g(1)");
        assert_eq!(span_slice(source, outer.expr_span), "f(g(1))");
        assert_eq!(
            inner.callee_span.lo,
            outer.callee_span.hi + 1,
            "inner callee starts right after the outer callee's '('"
        );
        assert_eq!(
            outer.expr_span.hi,
            inner.expr_span.hi + 1,
            "outer expr span extends one byte past the inner `)` to its own `)`"
        );
    }

    /// A preceding Unicode token shifts byte offsets away from zero, but the
    /// recorded spans still slice the exact callee and full call text.
    #[test]
    fn unicode_prefix_preserves_byte_offsets() {
        let source = "fn twice(x) { x + x }\nlet msg = \"変換\";\ntwice(1);\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");
        assert_eq!(index.call_sites.len(), 1);

        let site = &index.call_sites[0];
        let callee = span_slice(source, site.callee_span);
        let expr = span_slice(source, site.expr_span);
        assert_eq!(callee, "twice");
        assert_eq!(expr, "twice(1)");
        assert!(
            site.callee_span.lo > 0,
            "unicode-prefixed callee is not at byte zero"
        );
    }

    /// A direct local-callable call (`name(...)` where `name` binds a local)
    /// records exact callee + full-call slices, a distinct semantic id, and
    /// an honest `ParsedCallTarget::Local(slot)` — never a fabricated
    /// function index.
    #[test]
    fn local_callable_call_records_exact_slices_and_local_target() {
        use crate::compiler::ir::{ParsedCallTarget, SemanticNodeId};

        let source = "let twice = |x| x + x;\ntwice(21);\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");
        assert_eq!(
            index.call_sites.len(),
            1,
            "exactly one local call site recorded"
        );

        let site = &index.call_sites[0];
        assert_eq!(span_slice(source, site.callee_span), "twice");
        assert_eq!(span_slice(source, site.expr_span), "twice(21)");
        assert_eq!(
            site.expr_span.lo, site.callee_span.lo,
            "expr span starts at callee start"
        );
        match site.target {
            ParsedCallTarget::Local(slot) => assert_eq!(slot, 0, "first local is slot 0"),
            ref other => panic!("expected Local target, got {other:?}"),
        }
        assert!(
            !site.is_namespace_call,
            "plain local call is not a namespace call"
        );

        // The `Expr::LocalCall` node carries the same id.
        let local_calls: Vec<&Expr> = stmt_call_exprs(&ir)
            .into_iter()
            .filter(|expr| matches!(expr, Expr::LocalCall(..)))
            .collect();
        assert_eq!(
            local_calls.len(),
            1,
            "only the call statement is a LocalCall"
        );
        let Expr::LocalCall(_, _, _, semantic_id) = local_calls[0] else {
            panic!("stmt must be a LocalCall");
        };
        let Some(SemanticNodeId(id)) = semantic_id else {
            panic!("local call must carry a semantic id");
        };
        assert_eq!(SemanticNodeId(*id), site.id, "expr and site share one id");
    }

    /// A function-value reference (`f` without parens) must NOT be recorded
    /// as a call site, and calling through a local stays distinct from
    /// calling a named function on the same line.
    #[test]
    fn local_call_is_not_confused_with_function_value_reference() {
        use crate::compiler::ir::ParsedCallTarget;

        let source = "fn g(x) { x }\nlet f = g;\nf(1); g(2);\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");
        assert_eq!(
            index.call_sites.len(),
            2,
            "only the two call expressions are recorded"
        );

        let f_site = index
            .call_sites
            .iter()
            .find(|site| span_slice(source, site.callee_span) == "f")
            .expect("f call site");
        let g_site = index
            .call_sites
            .iter()
            .find(|site| span_slice(source, site.callee_span) == "g")
            .expect("g call site");
        assert!(
            matches!(f_site.target, ParsedCallTarget::Local(_)),
            "f(...) resolves through the local binding"
        );
        assert!(
            matches!(g_site.target, ParsedCallTarget::Function(_)),
            "g(...) resolves through the function table"
        );
        assert_ne!(
            f_site.id, g_site.id,
            "distinct call sites keep distinct ids"
        );
        assert_eq!(span_slice(source, f_site.expr_span), "f(1)");
        assert_eq!(span_slice(source, g_site.expr_span), "g(2)");
    }
}

#[cfg(test)]
mod lexical_scope_provenance_tests {
    use crate::compiler::CompileSourceFileOptions;
    use crate::compiler::source_map::Span;

    use super::{SourceFlavor, parse_source};

    fn parse(source: &str) -> crate::compiler::ir::FrontendIr {
        parse_source(
            source,
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("source must parse")
    }

    /// RustScript lowering is the identity, so span `.lo`/`.hi` are byte
    /// offsets into the original source string.
    fn span_slice(source: &str, span: Span) -> String {
        source
            .get(span.lo..span.hi)
            .expect("span must slice source")
            .to_string()
    }

    fn scopes_of(
        ir: &crate::compiler::ir::FrontendIr,
    ) -> &crate::compiler::ir::ParsedSemanticIndex {
        ir.parsed_semantic_index.as_ref().expect("index present")
    }
    /// Nested ordinary blocks (function body containing an if-block
    /// containing a while-block) produce a child scope for each `{...}`, with
    /// exact parent ids and `{...}` ranges, and declarations attach to the
    /// scope that lexically contains them in source order.
    #[test]
    fn nested_block_scopes_have_exact_parents_ranges_and_declaration_order() {
        let source = "fn f() {\n    let a = 0;\n    if a > 0 {\n        let b = 1;\n        while b < 2 {\n            let c = 2;\n        }\n    }\n    a;\n}\n";
        let ir = parse(source);
        let index = scopes_of(&ir);

        // scope 0 is the root (first token .. EOF).
        assert_eq!(
            index.scopes.len(),
            4,
            "root + fn body + if block + while block"
        );
        let root = &index.scopes[0];
        assert_eq!(root.parent, None, "root has no parent");
        assert_eq!(root.range.lo, 0, "root starts at first token");
        assert_eq!(root.range.hi, source.len(), "root ends at EOF");

        let fn_body = &index.scopes[1];
        let if_block = &index.scopes[2];
        let while_block = &index.scopes[3];
        assert_eq!(fn_body.parent, Some(0), "fn body parent is root");
        assert_eq!(if_block.parent, Some(1), "if block parent is fn body");
        assert_eq!(
            while_block.parent,
            Some(2),
            "while block parent is if block"
        );
        assert_eq!(
            span_slice(source, fn_body.range),
            "{\n    let a = 0;\n    if a > 0 {\n        let b = 1;\n        while b < 2 {\n            let c = 2;\n        }\n    }\n    a;\n}",
            "fn body range covers exact braces"
        );
        assert_eq!(
            span_slice(source, if_block.range),
            "{\n        let b = 1;\n        while b < 2 {\n            let c = 2;\n        }\n    }",
            "if block range covers exact braces"
        );
        assert_eq!(
            span_slice(source, while_block.range),
            "{\n            let c = 2;\n        }",
            "while block range covers exact braces"
        );
        assert!(
            fn_body.range.lo < if_block.range.lo && if_block.range.hi < fn_body.range.hi,
            "if block is nested inside the fn body"
        );
        assert!(
            if_block.range.lo < while_block.range.lo && while_block.range.hi < if_block.range.hi,
            "while block is nested inside the if block"
        );

        // Declarations: a in fn body; b in if block; c in while block.
        let a = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "a")
            .expect("a");
        let b = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "b")
            .expect("b");
        let c = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "c")
            .expect("c");
        assert_eq!(a.scope_id, 1);
        assert_eq!(b.scope_id, 2);
        assert_eq!(c.scope_id, 3);
        assert_eq!(a.decl_order, 0, "a is the first fn-body declaration");
        assert_eq!(b.decl_order, 0, "b is the first if-block declaration");
        assert_eq!(c.decl_order, 0, "c is the first while-block declaration");

        // The scope's own declaration vectors carry the recorded slots in
        // declaration order.
        assert_eq!(index.scopes[1].declarations.len(), 1);
        assert_eq!(index.scopes[2].declarations.len(), 1);
        assert_eq!(index.scopes[3].declarations.len(), 1);
    }

    /// Statement-form if/else arms are sibling scopes under the containing
    /// scope; a declaration in each arm lands in that arm's scope.
    #[test]
    fn if_else_arms_are_sibling_scopes() {
        let source = "fn f(x) {\n    if x > 0 {\n        let a = 1;\n    } else {\n        let b = 2;\n    }\n    x;\n}\n";
        let ir = parse(source);
        let index = scopes_of(&ir);

        // scope 1 = function body, scope 2 = then arm, scope 3 = else arm.
        assert_eq!(index.scopes.len(), 4, "root + fn body + two arms");
        let body = &index.scopes[1];
        let then_scope = &index.scopes[2];
        let else_scope = &index.scopes[3];
        assert_eq!(body.parent, Some(0), "fn body parent is root");
        assert_eq!(then_scope.parent, Some(1), "then arm parent is fn body");
        assert_eq!(else_scope.parent, Some(1), "else arm parent is fn body");
        assert_eq!(then_scope.id, 2);
        assert_eq!(else_scope.id, 3);
        assert_ne!(then_scope.id, else_scope.id, "arms are distinct scopes");
        assert_eq!(
            span_slice(source, then_scope.range),
            "{\n        let a = 1;\n    }",
            "then arm exact braces"
        );
        assert_eq!(
            span_slice(source, else_scope.range),
            "{\n        let b = 2;\n    }",
            "else arm exact braces"
        );
        assert!(
            then_scope.range.hi < else_scope.range.lo,
            "then arm text precedes else arm text"
        );

        let a = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "a")
            .expect("a");
        let b = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "b")
            .expect("b");
        assert_eq!(a.scope_id, 2, "a belongs to the then-arm scope");
        assert_eq!(b.scope_id, 3, "b belongs to the else-arm scope");
        assert_eq!(a.decl_order, 0);
        assert_eq!(b.decl_order, 0);
    }

    /// A while loop body is a child scope of the enclosing scope, and a
    /// declaration inside the body lands there.
    #[test]
    fn while_loop_body_is_a_child_scope() {
        let source = "let x = 0;\nwhile x < 10 {\n    let y = 5;\n}\n";
        let ir = parse(source);
        let index = scopes_of(&ir);

        assert_eq!(index.scopes.len(), 2, "root + loop body");
        let body = &index.scopes[1];
        assert_eq!(body.parent, Some(0), "loop body parent is root");
        assert_eq!(
            span_slice(source, body.range),
            "{\n    let y = 5;\n}",
            "loop body exact braces"
        );

        let y = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "y")
            .expect("y");
        assert_eq!(y.scope_id, 1, "y belongs to the loop body scope");
        assert_eq!(y.decl_order, 0);
    }

    /// Each match arm body is a sibling scope under the enclosing scope.
    #[test]
    fn match_arms_are_sibling_scopes() {
        let source = "fn f(x) {\n    match x {\n        1 => 10,\n        _ => 20,\n    }\n}\n";
        let ir = parse(source);
        let index = scopes_of(&ir);

        // scope 1 = fn body; scopes 2 and 3 = the two arm bodies.
        assert_eq!(index.scopes.len(), 4, "root + fn body + two match arms");
        let body = &index.scopes[1];
        let first_arm = &index.scopes[2];
        let second_arm = &index.scopes[3];
        assert_eq!(body.parent, Some(0));
        assert_eq!(first_arm.parent, Some(1), "first arm parent is fn body");
        assert_eq!(second_arm.parent, Some(1), "second arm parent is fn body");
        assert_ne!(first_arm.id, second_arm.id, "arms are distinct scopes");
        assert_eq!(
            span_slice(source, first_arm.range),
            "10",
            "first arm body exact expression span"
        );
        assert_eq!(
            span_slice(source, second_arm.range),
            "20",
            "second arm body exact expression span"
        );
        assert!(
            first_arm.range.hi <= second_arm.range.lo,
            "first arm text precedes second arm text"
        );
    }

    /// A closure body is a nested child scope of the enclosing scope.
    #[test]
    fn closure_body_is_a_nested_child_scope() {
        let source = "let f = |x| x + 1;\n";
        let ir = parse(source);
        let index = scopes_of(&ir);

        assert_eq!(index.scopes.len(), 2, "root + closure body");
        let closure_scope = &index.scopes[1];
        assert_eq!(closure_scope.parent, Some(0), "closure body parent is root");
        assert_eq!(
            span_slice(source, closure_scope.range),
            "x + 1",
            "closure body exact expression span"
        );
    }

    /// Function declarations recorded at the enclosing scope keep real
    /// declaration order in the scope's `functions` vector and in
    /// `decl_order` on each site.
    #[test]
    fn top_level_function_declarations_are_recorded_in_order() {
        let source = "fn a() { 1 }\nfn b() { 2 }\n";
        let ir = parse(source);
        let index = scopes_of(&ir);

        // scope 1 = fn a body, scope 2 = fn b body; both parent root.
        assert_eq!(index.scopes.len(), 3, "root + two fn bodies");
        assert_eq!(index.scopes[1].parent, Some(0));
        assert_eq!(index.scopes[2].parent, Some(0));

        let a_decl = index
            .func_decls
            .iter()
            .find(|decl| decl.name == "a")
            .expect("a decl");
        let b_decl = index
            .func_decls
            .iter()
            .find(|decl| decl.name == "b")
            .expect("b decl");
        assert_eq!(a_decl.scope_id, 0, "fn a declared at root");
        assert_eq!(b_decl.scope_id, 0, "fn b declared at root");
        assert_eq!(a_decl.decl_order, 0, "fn a is the first root function");
        assert_eq!(b_decl.decl_order, 1, "fn b is the second root function");

        assert_eq!(
            index.scopes[0].functions,
            vec![a_decl.function_index, b_decl.function_index],
            "root functions vector is in declaration order"
        );
    }
}

/// Full parser provenance for every source binding and reference: function
/// params, closure params, for/map/match bindings, assignment/increment/
/// index-assignment targets, local-call callees, direct function callees and
/// function-value references — each with exact identifier token spans, the
/// resolved local slot / function index, the lexical scope id, and coherent
/// declaration order.
#[cfg(test)]
mod parser_binding_provenance_tests {
    use crate::compiler::ir::FrontendIr;
    use crate::compiler::parser::ParserDialect;
    use crate::compiler::source_map::Span;
    use crate::compiler::{CompileSourceFileOptions, SharedParserOptions};

    use super::{SourceFlavor, parse_source, parse_source_with_dialect};

    fn parse(source: &str) -> FrontendIr {
        parse_source(
            source,
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("source must parse")
    }

    /// RustScript lowering is the identity, so span `.lo`/`.hi` are byte
    /// offsets into the original source string.
    fn span_slice(source: &str, span: Span) -> String {
        source
            .get(span.lo..span.hi)
            .expect("span must slice source")
            .to_string()
    }

    /// A test dialect that additionally enables arrow-closure and increment
    /// syntax so those binding/ref sites can be exercised under the shared
    /// expression parser (the default RustScript dialect disables them).
    struct MaximalDialect;
    impl ParserDialect for MaximalDialect {
        fn allow_let_mut_binding(&self) -> bool {
            true
        }
        fn allow_plus_equal_operator(&self) -> bool {
            true
        }
        fn allow_for_in_loop(&self) -> bool {
            true
        }
        fn allow_arrow_closure(&self) -> bool {
            true
        }
        fn allow_increment_operator(&self) -> bool {
            true
        }
    }
    static MAXIMAL_DIALECT: MaximalDialect = MaximalDialect;

    fn parse_with_dialect(source: &str) -> FrontendIr {
        parse_source_with_dialect(
            source,
            &MAXIMAL_DIALECT,
            SharedParserOptions {
                source_id: 0,
                allow_implicit_externs: false,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: false,
            },
        )
        .expect("source must parse")
    }

    fn index(ir: &FrontendIr) -> &crate::compiler::ir::ParsedSemanticIndex {
        ir.parsed_semantic_index.as_ref().expect("index present")
    }

    fn decls<'i>(
        i: &'i crate::compiler::ir::ParsedSemanticIndex,
        name: &str,
    ) -> Vec<&'i crate::compiler::ir::LocalDeclSite> {
        i.local_decls
            .iter()
            .filter(|d| d.name == name)
            .collect::<Vec<_>>()
    }

    fn refs<'i>(
        i: &'i crate::compiler::ir::ParsedSemanticIndex,
        name: &str,
    ) -> Vec<&'i crate::compiler::ir::LocalRefSite> {
        i.local_refs
            .iter()
            .filter(|r| r.name == name)
            .collect::<Vec<_>>()
    }

    /// Function parameters record exact local declarations (ident spans,
    /// slot, body scope, decl order) and their uses inside the body record
    /// local references resolving to the same slots.
    #[test]
    fn function_params_are_decl_sites_and_body_uses_are_refs() {
        let source = "fn add(a, b) { a + b }\nadd(1, 2);\n";
        let ir = parse(source);
        let i = index(&ir);

        let a = decls(i, "a");
        let b = decls(i, "b");
        assert_eq!(a.len(), 1, "one `a` decl");
        assert_eq!(b.len(), 1, "one `b` decl");
        assert_eq!(span_slice(source, a[0].ident_span), "a");
        assert_eq!(span_slice(source, b[0].ident_span), "b");
        assert_ne!(a[0].slot, b[0].slot, "params take distinct slots");
        assert_eq!(a[0].scope_id, 1, "params live in the fn body scope");
        assert_eq!(b[0].scope_id, 1);
        assert_eq!(a[0].decl_order, 0, "a is the first body declaration");
        assert_eq!(b[0].decl_order, 1, "b is the second body declaration");
        assert_eq!(i.scopes[1].declarations, vec![a[0].slot, b[0].slot]);

        // Body uses `a` and `b` resolve to the param slots.
        let a_refs = refs(i, "a");
        let b_refs = refs(i, "b");
        assert_eq!(a_refs.len(), 1);
        assert_eq!(b_refs.len(), 1);
        assert_eq!(a_refs[0].slot, a[0].slot);
        assert_eq!(b_refs[0].slot, b[0].slot);
        assert_eq!(span_slice(source, a_refs[0].ident_span), "a");
        assert_eq!(span_slice(source, b_refs[0].ident_span), "b");

        // The direct function callee is both a call site and a function ref.
        let callee_refs = i
            .func_refs
            .iter()
            .filter(|r| r.name == "add")
            .collect::<Vec<_>>();
        assert_eq!(
            callee_refs.len(),
            1,
            "direct `add(1, 2)` callee is one func ref"
        );
        assert_eq!(span_slice(source, callee_refs[0].ident_span), "add");
    }

    /// Closure parameters record local declarations inside the closure body
    /// scope, and uses in the body resolve to the param slot.
    #[test]
    fn closure_params_are_decl_sites_for_pipe_and_arrow_forms() {
        // Pipe closure.
        let pipe = "let f = |x| x + 1;\n";
        let ir = parse(pipe);
        let i = index(&ir);
        let x = decls(i, "x");
        assert_eq!(x.len(), 1, "one pipe-closure `x` decl");
        assert_eq!(span_slice(pipe, x[0].ident_span), "x");
        assert_eq!(x[0].scope_id, 1, "closure body is the child scope");
        assert_eq!(i.scopes[1].declarations, vec![x[0].slot]);
        let x_refs = refs(i, "x");
        assert_eq!(x_refs.len(), 1);
        assert_eq!(
            x_refs[0].slot, x[0].slot,
            "`x` use resolves to the param slot"
        );

        // Arrow closure (enabled by the maximal test dialect).
        let arrow = "let g = a => a * 2;\n";
        let ir = parse_with_dialect(arrow);
        let i = index(&ir);
        let a = decls(i, "a");
        assert_eq!(a.len(), 1, "one arrow-closure `a` decl");
        assert_eq!(span_slice(arrow, a[0].ident_span), "a");
        assert_eq!(a[0].scope_id, 1);
    }

    /// The range-for iterator binding and the map iterator key/value bindings
    /// each record a local declaration site with the exact identifier span.
    #[test]
    fn for_range_and_map_iterator_bindings_are_decl_sites() {
        let source = "let mut total = 0;\nfor i in 0..3 { total = total + i; }\n";
        let ir = parse(source);
        let i = index(&ir);
        let i_decl = decls(i, "i");
        assert_eq!(i_decl.len(), 1, "one range-for `i` decl");
        assert_eq!(span_slice(source, i_decl[0].ident_span), "i");
        assert_eq!(i_decl[0].scope_id, 0, "iterator binds in the root scope");
        // The iterator body use resolves to the same slot.
        let i_refs = refs(i, "i");
        assert_eq!(i_refs.len(), 1);
        assert_eq!(i_refs[0].slot, i_decl[0].slot);

        // Map iteration: `for (key, value) in &map`.
        let map_src = "let m = {};\nfor (key, value) in &m { value; }\n";
        let ir = parse(map_src);
        let i = index(&ir);
        let key = decls(i, "key");
        let value = decls(i, "value");
        assert_eq!(key.len(), 1, "one map `key` decl");
        assert_eq!(value.len(), 1, "one map `value` decl");
        assert_eq!(span_slice(map_src, key[0].ident_span), "key");
        assert_eq!(span_slice(map_src, value[0].ident_span), "value");
        assert_ne!(key[0].slot, value[0].slot);
        assert_eq!(key[0].scope_id, 0);
        assert_eq!(value[0].scope_id, 0);
    }

    /// A match arm binding (`Some(x) => x`) records a local declaration inside
    /// the arm body scope, and the body use resolves to that slot.
    #[test]
    fn match_pattern_binding_is_a_decl_site_in_the_arm_scope() {
        let source = "fn f(x) { match x { Some(v) => v, _ => 0 } }\n";
        let ir = parse(source);
        let i = index(&ir);
        let v = decls(i, "v");
        assert_eq!(v.len(), 1, "one match-arm `v` decl");
        assert_eq!(span_slice(source, v[0].ident_span), "v");
        // scope 1 = fn body, scope 2 = the Some-arm body.
        assert_eq!(v[0].scope_id, 2, "binding lives in the arm body scope");
        assert_eq!(i.scopes[2].declarations, vec![v[0].slot]);
        let v_refs = refs(i, "v");
        assert_eq!(v_refs.len(), 1);
        assert_eq!(v_refs[0].slot, v[0].slot);
        assert_eq!(span_slice(source, v_refs[0].ident_span), "v");
    }

    /// Assignment targets, prefix+statement increments, and index-assignment
    /// roots are recorded as local references with exact identifier spans.
    #[test]
    fn mutation_targets_are_local_references() {
        let source = "let mut x = 0;\nlet mut a = [0];\nx = 1;\n++x;\na[0] = 2;\n";
        let ir = parse_with_dialect(source);
        let i = index(&ir);

        // `x = 1` target.
        let x_refs = refs(i, "x");
        assert!(
            x_refs.len() >= 2,
            "assignment plus increment targets both reference x"
        );
        assert!(
            x_refs
                .iter()
                .any(|r| span_slice(source, r.ident_span) == "x"),
            "assignment target x recorded"
        );

        // `a[0] = 2` index-assignment root.
        let a_refs = refs(i, "a");
        assert!(
            a_refs
                .iter()
                .any(|r| span_slice(source, r.ident_span) == "a"),
            "index-assignment root a recorded"
        );
    }

    /// A closure parameter shadowing an outer `let` resolves to a distinct
    /// slot; references inside the closure body point at the inner binding,
    /// references outside point at the outer one.
    #[test]
    fn shadowed_names_map_to_distinct_slots_and_resolve_per_scope() {
        let source = "let x = 1;\nlet f = |x| x;\nf(2);\nx;\n";
        let ir = parse(source);
        let i = index(&ir);

        let x_decls = decls(i, "x");
        assert_eq!(x_decls.len(), 2, "outer `let x` and closure param `x`");
        let outer = x_decls.iter().find(|d| d.scope_id == 0).expect("outer x");
        let inner = x_decls.iter().find(|d| d.scope_id == 1).expect("inner x");
        assert_ne!(outer.slot, inner.slot, "shadowing yields a distinct slot");

        // Closure-body `x` resolves to the inner slot, trailing `x;` to the
        // outer slot.
        let x_refs = refs(i, "x");
        assert_eq!(x_refs.len(), 2, "body use + trailing top-level use");
        assert!(
            x_refs.iter().any(|r| r.slot == inner.slot),
            "closure-body `x` resolves to the inner slot"
        );
        assert!(
            x_refs.iter().any(|r| r.slot == outer.slot),
            "top-level `x;` resolves to the outer slot"
        );
    }

    /// A direct function callee and a bare function-value reference both
    /// record FunctionRefSite entries with exact spans and the same index,
    /// distinguishable from one another by source position.
    #[test]
    fn function_callee_and_function_value_refs_have_exact_spans() {
        let source = "fn g(x) { x }\ng(1);\nlet h = g;\n";
        let ir = parse(source);
        let i = index(&ir);

        let g_refs = i
            .func_refs
            .iter()
            .filter(|r| r.name == "g")
            .collect::<Vec<_>>();
        assert_eq!(g_refs.len(), 2, "one callee ref + one value ref");

        let callee = g_refs[0];
        let value = g_refs[1];
        assert!(
            callee.ident_span.lo < value.ident_span.lo,
            "callee precedes value"
        );
        assert_eq!(callee.target, value.target, "same function target");
        assert_eq!(span_slice(source, callee.ident_span), "g");
        assert_eq!(span_slice(source, value.ident_span), "g");
    }
}

/// Exact provenance-span remapping from lowered RustScript back to the
/// original source.
///
/// The RustScript frontend lowers through [`LoweringBuilder`], which records
/// a byte-for-byte mapping while the lowered text is produced. Every span in
/// the parsed semantic index must reference the original source id and slice
/// the intended original call/local/function/scope text — never the lowered
/// text, never a guessed offset.
#[cfg(test)]
mod lowered_provenance_remap_tests {
    use crate::compiler::frontends::rustscript;
    use crate::compiler::source_map::{LoweredSource, LoweringBuilder, Span};
    use crate::compiler::{CompileSourceFileOptions, ReplLocalBinding, SourceFlavor};

    use super::{parse_lowered_with_mapping, parse_rustscript_repl_source, parse_source};

    fn span_slice(source: &str, span: Span) -> String {
        source
            .get(span.lo..span.hi)
            .expect("span must slice source")
            .to_string()
    }

    /// Identity lowering: every provenance span carries the original source
    /// id and slices the exact original call/local/function/scope text.
    #[test]
    fn identity_lowering_maps_every_provenance_span_to_original() {
        let source = "fn add(a, b) { a + b }\nlet msg = \"変換\";\nadd(msg, 2);\n";
        let ir = parse_source(
            source,
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("source must parse");
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        // Every call site references the original source and slices exactly.
        for site in &index.call_sites {
            assert_eq!(site.callee_span.source_id, 0, "callee span is original");
            assert_eq!(site.expr_span.source_id, 0, "expr span is original");
        }
        let add_site = index
            .call_sites
            .iter()
            .find(|site| site.name == "add")
            .expect("add call site");
        assert_eq!(span_slice(source, add_site.callee_span), "add");
        assert_eq!(span_slice(source, add_site.expr_span), "add(msg, 2)");

        // Local declarations and references slice the original identifier.
        for decl in &index.local_decls {
            assert_eq!(decl.ident_span.source_id, 0, "decl ident is original");
            assert_eq!(decl.stmt_span.source_id, 0, "decl stmt is original");
            assert_eq!(span_slice(source, decl.ident_span), decl.name);
        }
        for reference in &index.local_refs {
            assert_eq!(reference.ident_span.source_id, 0, "ref ident is original");
            assert_eq!(span_slice(source, reference.ident_span), reference.name);
        }

        // Function declarations and value references slice the original name.
        for decl in &index.func_decls {
            assert_eq!(decl.ident_span.source_id, 0, "func decl is original");
            assert_eq!(span_slice(source, decl.ident_span), decl.name);
        }
        for reference in &index.func_refs {
            assert_eq!(reference.ident_span.source_id, 0, "func ref is original");
            assert_eq!(span_slice(source, reference.ident_span), reference.name);
        }

        // Lexical scopes slice original braces/expression ranges.
        for scope in &index.scopes {
            assert_eq!(scope.range.source_id, 0, "scope range is original");
        }
        assert_eq!(span_slice(source, index.scopes[0].range), source);
        let body = &index.scopes[1];
        assert_eq!(
            span_slice(source, body.range),
            "{ a + b }",
            "fn body range covers exact original braces"
        );
    }

    /// Unicode bytes before a target do not disturb the exact remap: spans
    /// still reference the original source and slice the intended text.
    #[test]
    fn unicode_prefix_maps_to_exact_original_slices() {
        let source = "let msg = \"変換\";\nprint(msg);\n";
        let ir = parse_source(
            source,
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("source must parse");
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let site = index
            .call_sites
            .iter()
            .find(|site| site.name == "print")
            .expect("print call site");
        assert_eq!(site.callee_span.source_id, 0);
        assert_eq!(span_slice(source, site.callee_span), "print");
        assert_eq!(span_slice(source, site.expr_span), "print(msg)");

        let msg_decl = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "msg")
            .expect("msg decl");
        assert_eq!(msg_decl.ident_span.source_id, 0);
        assert_eq!(span_slice(source, msg_decl.ident_span), "msg");

        let msg_ref = index
            .local_refs
            .iter()
            .find(|reference| reference.name == "msg")
            .expect("msg ref");
        assert_eq!(span_slice(source, msg_ref.ident_span), "msg");
        assert!(
            msg_ref.ident_span.lo > 0,
            "unicode-prefixed ref is not at byte zero"
        );
    }

    /// Build a `LoweredSource` through [`LoweringBuilder`] with a real
    /// transformation (a prefix comment inserted before a `let` statement and
    /// a multi-byte Unicode string kept verbatim), then parse the lowered
    /// text through the same `parse_lowered_with_mapping` path the frontend
    /// uses. Every provenance span must map to the exact original slice,
    /// including the offset shift caused by the inserted text.
    #[test]
    fn transformed_lowering_maps_provenance_to_exact_original_slices() {
        let original = "let msg = \"変換\";\nprint(msg);\n";
        let mut builder = LoweringBuilder::new(original);
        // Insert lowered-only comment text before the original first token.
        builder.insert("// lowered prefix\n");
        builder.copy_rest();
        let lowered = builder.finish();
        assert_eq!(
            lowered.text,
            "// lowered prefix\nlet msg = \"変換\";\nprint(msg);\n"
        );
        assert!(
            lowered.byte_mapping.map_offset(lowered.text.len()).unwrap() == original.len(),
            "trailing offset maps to original EOF"
        );

        let ir = parse_lowered_with_mapping(original, lowered, false, false, true, 7, None)
            .expect("lowered source must parse");
        let index = ir.parsed_semantic_index.as_ref().expect("index present");
        assert!(
            index.call_sites.len() == 1 && index.local_decls.len() == 1,
            "lowered parse records the call and the decl"
        );

        // The call site is at a shifted lowered offset; it must remap to the
        // exact original `print(msg)` slice with the original source id.
        let site = &index.call_sites[0];
        assert_eq!(site.callee_span.source_id, 7, "original source id kept");
        assert_eq!(site.expr_span.source_id, 7, "original source id kept");
        assert_eq!(span_slice(original, site.callee_span), "print");
        assert_eq!(span_slice(original, site.expr_span), "print(msg)");

        let decl = &index.local_decls[0];
        assert_eq!(decl.ident_span.source_id, 7);
        assert_eq!(span_slice(original, decl.ident_span), "msg");
        assert_eq!(
            span_slice(original, decl.stmt_span),
            "msg = \"変換\";",
            "stmt span starts at the ident and slices the original statement tail"
        );

        let reference = &index.local_refs[0];
        assert_eq!(reference.ident_span.source_id, 7);
        assert_eq!(span_slice(original, reference.ident_span), "msg");

        // The scope tree maps the root and fn-body ranges onto the original.
        for scope in &index.scopes {
            assert_eq!(scope.range.source_id, 7, "scope range is original");
        }
        assert_eq!(span_slice(original, index.scopes[0].range), original);
    }

    /// The REPL parse path uses the same exact byte remap: provenance spans
    /// reference the original snippet, not the lowered copy.
    #[test]
    fn repl_lowered_parse_maps_provenance_to_original_snippet() {
        let source = "let x = 1;\nx + 1;\n";
        let parsed = parse_rustscript_repl_source(source, &[]).expect("repl source must parse");
        let index = parsed
            .ir
            .parsed_semantic_index
            .as_ref()
            .expect("repl index present");

        let x_decl = index
            .local_decls
            .iter()
            .find(|decl| decl.name == "x")
            .expect("x decl");
        assert_eq!(x_decl.ident_span.source_id, 0, "repl decl is original");
        assert_eq!(span_slice(source, x_decl.ident_span), "x");
        assert_eq!(span_slice(source, x_decl.stmt_span), "x = 1;");

        let x_ref = index
            .local_refs
            .iter()
            .find(|reference| reference.name == "x")
            .expect("x ref");
        assert_eq!(x_ref.ident_span.source_id, 0, "repl ref is original");
        assert_eq!(span_slice(source, x_ref.ident_span), "x");

        for scope in &index.scopes {
            assert_eq!(scope.range.source_id, 0, "repl scope is original");
        }
        assert_eq!(span_slice(source, index.scopes[0].range), source);
    }

    /// The frontend `lower` entry produces a byte-exact identity mapping: the
    /// lowered text equals the input and every byte offset maps to itself,
    /// including offsets inside multi-byte UTF-8 sequences (never splitting a
    /// code point's bytes).
    #[test]
    fn frontend_lower_produces_byte_exact_identity_mapping() {
        let source = "fn 変換(x) { x }\n変換(1);\n";
        let lowered: LoweredSource = rustscript::lower(source).expect("lower succeeds");
        assert_eq!(lowered.text, source, "identity lowering is byte-exact");
        for offset in 0..=source.len() {
            assert_eq!(
                lowered.byte_mapping.map_offset(offset),
                Some(offset),
                "identity maps byte offset {offset} to itself"
            );
        }
    }

    /// Predeclared REPL locals do not disturb the exact remap of the snippet's
    /// own provenance spans.
    #[test]
    fn repl_with_predeclared_locals_still_maps_exactly() {
        let source = "x + 1;\n";
        let predefined = vec![ReplLocalBinding {
            name: "x".to_string(),
            mutable: false,
            schema: None,
            optional: false,
        }];
        let parsed = parse_rustscript_repl_source(source, &predefined).expect("repl parse ok");
        let index = parsed
            .ir
            .parsed_semantic_index
            .as_ref()
            .expect("repl index present");
        let x_ref = index
            .local_refs
            .iter()
            .find(|reference| reference.name == "x")
            .expect("x ref");
        assert_eq!(x_ref.ident_span.source_id, 0);
        assert_eq!(span_slice(source, x_ref.ident_span), "x");
        assert_eq!(index.scopes[0].range.source_id, 0);
    }
}

/// Provenance for every direct postfix source form: index get, member get,
/// `.length`, `.has`/`.keys`, slices, `.unwrap_or`, and `?.` optional access.
/// Each form records a `Some` semantic id plus a call site with a truthful
/// callee span (operator/member/key token range) and the full postfix
/// expression span; compiler-synthetic lowering (array/map literal builtins,
/// slice helper `Len` calls) keeps `None` ids.
#[cfg(test)]
mod postfix_provenance_tests {
    use crate::compiler::ir::{Expr, ParsedCallTarget, SemanticNodeId, Stmt};
    use crate::compiler::source_map::Span;
    use crate::compiler::{CompileSourceFileOptions, SharedParserOptions};

    use super::{SourceFlavor, parse_source, parse_source_with_dialect};

    fn parse(source: &str) -> crate::compiler::ir::FrontendIr {
        parse_source(
            source,
            SourceFlavor::RustScript,
            &CompileSourceFileOptions::default(),
        )
        .expect("source must parse")
    }

    fn span_slice(source: &str, span: Span) -> String {
        source
            .get(span.lo..span.hi)
            .expect("span must slice source")
            .to_string()
    }

    fn site<'i>(
        index: &'i crate::compiler::ir::ParsedSemanticIndex,
        name: &str,
    ) -> &'i crate::compiler::ir::ParsedCallSite {
        index
            .call_sites
            .iter()
            .find(|site| site.name == name)
            .unwrap_or_else(|| panic!("no call site named {name:?}"))
    }

    /// Index get (`arr[0]`) records the `[0]` operator range as callee, the
    /// full `arr[0]` as expr span, a distinct id, and a builtin `Get` target;
    /// the array literal's synthetic `ArrayNew`/`ArrayPush` calls stay `None`.
    #[test]
    fn index_get_records_exact_operator_and_expr_slices() {
        let source = "let arr = [1, 2];\narr[0];\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let get = site(index, "get");
        assert_eq!(span_slice(source, get.callee_span), "[0]", "operator range");
        assert_eq!(span_slice(source, get.expr_span), "arr[0]", "full expr");
        assert!(
            get.expr_span.lo < get.callee_span.lo,
            "expr starts at `arr`"
        );
        match get.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::Get.call_index())
            }
            ref other => panic!("expected builtin Get target, got {other:?}"),
        }

        // The `Expr::Call` node for the get carries the same id; the array
        // literal synthetic calls carry `None`.
        let mut synthetic_none = 0usize;
        let mut get_node: Option<SemanticNodeId> = None;
        for stmt in &ir.stmts {
            if let Stmt::Let { expr, .. } = stmt {
                for (_, id) in collect_call_ids(expr) {
                    if id.is_none() {
                        synthetic_none += 1;
                    }
                }
            }
            if let Stmt::Expr { expr, .. } = stmt {
                if let Expr::Call(_, _, _, _, id) = expr {
                    get_node = *id;
                }
            }
        }
        assert_eq!(get_node, Some(get.id), "get node shares the site id");
        assert_eq!(
            synthetic_none, 3,
            "ArrayNew + two ArrayPush calls stay None"
        );
    }

    /// A chained postfix (`arr[0].length`) records one site per step with
    /// exact slices: the inner index covers `arr[0]` and the outer `.length`
    /// covers `arr[0].length`.
    #[test]
    fn chained_index_and_length_record_exact_steps() {
        let source = "let arr = [1, 2];\narr[0].length;\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let get = site(index, "get");
        let length = site(index, "length");
        assert_eq!(span_slice(source, get.callee_span), "[0]");
        assert_eq!(span_slice(source, get.expr_span), "arr[0]");
        assert_eq!(span_slice(source, length.callee_span), "length");
        assert_eq!(span_slice(source, length.expr_span), "arr[0].length");
        assert_ne!(get.id, length.id, "each step gets a distinct id");
        assert_eq!(
            get.expr_span.lo, length.expr_span.lo,
            "both steps start at the chain base"
        );
        assert!(
            get.expr_span.hi < length.callee_span.lo,
            "inner expr ends before the outer member"
        );
        match length.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::Len.call_index())
            }
            ref other => panic!("expected Len target, got {other:?}"),
        }
    }

    /// `.has(k)` and `.keys` record the member token as callee and the full
    /// postfix expression as expr span.
    #[test]
    fn has_and_keys_record_member_callee_and_full_expr() {
        let source = "let m = {}; let k = 1;\nm.has(k);\nm.keys;\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let has = site(index, "has");
        assert_eq!(span_slice(source, has.callee_span), "has");
        assert_eq!(span_slice(source, has.expr_span), "m.has(k)");
        match has.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::Has.call_index())
            }
            ref other => panic!("expected Has target, got {other:?}"),
        }

        let keys = site(index, "keys");
        assert_eq!(span_slice(source, keys.callee_span), "keys");
        assert_eq!(span_slice(source, keys.expr_span), "m.keys");
        match keys.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::Keys.call_index())
            }
            ref other => panic!("expected Keys target, got {other:?}"),
        }
    }

    /// A slice (`s[1:3]`) records the `[1:3]` bracket range and the full
    /// `s[1:3]` expr span, and the operative `Slice` call carries the id
    /// while the lowering's synthetic `Len` helper stays `None`.
    #[test]
    fn slice_records_bracket_callee_and_operative_call_id() {
        let source = "let s = [1, 2, 3];\ns[1:3];\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let slice = site(index, "slice");
        assert_eq!(span_slice(source, slice.callee_span), "[1:3]");
        assert_eq!(span_slice(source, slice.expr_span), "s[1:3]");
        match slice.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::Slice.call_index())
            }
            ref other => panic!("expected Slice target, got {other:?}"),
        }

        // Find the operative Slice call inside the lowered Match chain and
        // assert it carries the site id; the synthetic Len call stays None.
        let expr = ir
            .stmts
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::Expr { expr, .. } => Some(expr),
                _ => None,
            })
            .expect("expr stmt");
        let slice_calls = collect_call_ids(expr)
            .into_iter()
            .filter(|(index, _)| *index == crate::builtins::BuiltinFunction::Slice.call_index())
            .collect::<Vec<_>>();
        assert!(!slice_calls.is_empty(), "slice call present in lowered IR");
        assert!(
            slice_calls.iter().any(|(_, id)| *id == Some(slice.id)),
            "operative Slice call carries the site id"
        );
        let len_calls = collect_call_ids(expr)
            .into_iter()
            .filter(|(index, _)| *index == crate::builtins::BuiltinFunction::Len.call_index())
            .collect::<Vec<_>>();
        assert!(!len_calls.is_empty(), "synthetic Len call present");
        for (_, id) in &len_calls {
            assert_eq!(*id, None, "synthetic Len helper stays None");
        }
    }

    /// `.unwrap_or(d)` records the member token as callee, the full
    /// `o.unwrap_or(5)` expr span, an `Unresolved` target, and the
    /// `OptionUnwrapOr` node carries the same id.
    #[test]
    fn unwrap_or_records_member_callee_and_node_id() {
        let source = "let o = null;\no.unwrap_or(5);\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let unwrap = site(index, "unwrap_or");
        assert_eq!(span_slice(source, unwrap.callee_span), "unwrap_or");
        assert_eq!(span_slice(source, unwrap.expr_span), "o.unwrap_or(5)");
        assert!(matches!(unwrap.target, ParsedCallTarget::Unresolved));

        let expr = ir
            .stmts
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::Expr { expr, .. } => Some(expr),
                _ => None,
            })
            .expect("expr stmt");
        match expr {
            Expr::OptionUnwrapOr { semantic_id, .. } => {
                assert_eq!(*semantic_id, Some(unwrap.id), "node shares site id")
            }
            other => panic!("expected OptionUnwrapOr, got {other:?}"),
        }
    }

    /// Optional access (`x?.y` and `x?.[k]`) records the member/key range as
    /// callee, the full postfix expr span, and the `OptionalGet` node carries
    /// the same id.
    #[test]
    fn optional_access_records_member_callee_and_node_id() {
        let source = "let x = null; let k = 1;\nx?.y;\nx?.[k];\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let member_sites = index
            .call_sites
            .iter()
            .filter(|site| span_slice(source, site.callee_span) == "y")
            .collect::<Vec<_>>();
        assert_eq!(member_sites.len(), 1, "one member access site");
        let member_site = member_sites[0];
        assert_eq!(span_slice(source, member_site.expr_span), "x?.y");

        let index_sites = index
            .call_sites
            .iter()
            .filter(|site| span_slice(source, site.callee_span) == "[k]")
            .collect::<Vec<_>>();
        assert_eq!(index_sites.len(), 1, "one optional index site");
        let index_site = index_sites[0];
        assert_eq!(span_slice(source, index_site.expr_span), "x?.[k]");
        assert_ne!(member_site.id, index_site.id);

        let exprs = ir
            .stmts
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Expr { expr, .. } => Some(expr),
                _ => None,
            })
            .collect::<Vec<_>>();
        match exprs[0] {
            Expr::OptionalGet { semantic_id, .. } => {
                assert_eq!(*semantic_id, Some(member_site.id))
            }
            other => panic!("expected OptionalGet, got {other:?}"),
        }
        match exprs[1] {
            Expr::OptionalGet { semantic_id, .. } => {
                assert_eq!(*semantic_id, Some(index_site.id))
            }
            other => panic!("expected OptionalGet, got {other:?}"),
        }
    }

    /// Member get (`m.foo`) is a direct source expression: it records the
    /// member token as callee and the full chain as expr span.
    #[test]
    fn member_get_records_exact_callee_and_expr() {
        let source = "let m = {};\nm.foo;\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let foo = site(index, "foo");
        assert_eq!(span_slice(source, foo.callee_span), "foo");
        assert_eq!(span_slice(source, foo.expr_span), "m.foo");
        match foo.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::Get.call_index())
            }
            ref other => panic!("expected Get target, got {other:?}"),
        }
    }

    /// Collect every `(call index, semantic id)` pair under an expression,
    /// including nested calls inside `Match`/`IfElse`/arithmetic wrappers.
    fn collect_call_ids(expr: &Expr) -> Vec<(u16, Option<SemanticNodeId>)> {
        let mut out = Vec::new();
        fn walk(expr: &Expr, out: &mut Vec<(u16, Option<SemanticNodeId>)>) {
            match expr {
                Expr::Call(index, _, args, _, id) => {
                    out.push((*index, *id));
                    for arg in args {
                        walk(arg, out);
                    }
                }
                Expr::LocalCall(_, _, args, _) | Expr::ModuleCall(_, _, args, _) => {
                    for arg in args {
                        walk(arg, out);
                    }
                }
                Expr::OptionalGet { container, key, .. } => {
                    walk(container, out);
                    walk(key, out);
                }
                Expr::OptionUnwrapOr {
                    value, fallback, ..
                } => {
                    walk(value, out);
                    walk(fallback, out);
                }
                Expr::IfElse {
                    condition,
                    then_expr,
                    else_expr,
                } => {
                    walk(condition, out);
                    walk(then_expr, out);
                    walk(else_expr, out);
                }
                Expr::Match {
                    value,
                    arms,
                    default,
                    ..
                } => {
                    walk(value, out);
                    for (_, arm) in arms {
                        walk(arm, out);
                    }
                    walk(default, out);
                }
                Expr::Add(lhs, rhs)
                | Expr::Sub(lhs, rhs)
                | Expr::Mul(lhs, rhs)
                | Expr::Div(lhs, rhs)
                | Expr::Mod(lhs, rhs)
                | Expr::And(lhs, rhs)
                | Expr::Or(lhs, rhs)
                | Expr::Eq(lhs, rhs)
                | Expr::Lt(lhs, rhs)
                | Expr::Gt(lhs, rhs) => {
                    walk(lhs, out);
                    walk(rhs, out);
                }
                Expr::Neg(inner) | Expr::Not(inner) | Expr::ToOwned(inner) => walk(inner, out),
                Expr::Block { stmts, expr } => {
                    for stmt in stmts {
                        if let Stmt::Let { expr, .. } = stmt {
                            walk(expr, out);
                        }
                        if let Stmt::Expr { expr, .. } = stmt {
                            walk(expr, out);
                        }
                    }
                    walk(expr, out);
                }
                _ => {}
            }
        }
        walk(expr, &mut out);
        out
    }

    /// A test dialect that enables dotted JS-style calls so the
    /// `console.log(...)` / builtin-dotted provenance path is exercised.
    struct DottedDialect;
    impl crate::compiler::parser::ParserDialect for DottedDialect {
        fn allow_dotted_call(&self) -> bool {
            true
        }
    }
    static DOTTED_DIALECT: DottedDialect = DottedDialect;

    /// Builtin namespace calls (`json::encode(...)`, `math::abs(...)`) record
    /// the exact path callee and the full call expr span.
    #[test]
    fn builtin_namespace_calls_record_exact_path_provenance() {
        let source = "use json;\nuse math;\nlet s = \"{}\";\njson::encode(s);\nmath::abs(-1);\n";
        let ir = parse(source);
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let encode = index
            .call_sites
            .iter()
            .find(|site| site.name == "json::encode")
            .expect("json::encode site");
        assert_eq!(span_slice(source, encode.callee_span), "json::encode");
        assert_eq!(span_slice(source, encode.expr_span), "json::encode(s)");
        assert!(encode.is_namespace_call, "namespace call flagged");
        match encode.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::JsonEncode.call_index())
            }
            ref other => panic!("expected builtin target, got {other:?}"),
        }

        let abs = index
            .call_sites
            .iter()
            .find(|site| site.name == "math::abs")
            .expect("math::abs site");
        assert_eq!(span_slice(source, abs.callee_span), "math::abs");
        assert_eq!(span_slice(source, abs.expr_span), "math::abs(-1)");
        assert!(abs.is_namespace_call);
        match abs.target {
            ParsedCallTarget::Function(i) => {
                assert_eq!(i, crate::builtins::BuiltinFunction::MathAbs.call_index())
            }
            ref other => panic!("expected builtin target, got {other:?}"),
        }
    }

    /// Dotted JS calls (`console.log(...)`) record the dotted path callee and
    /// the full call expr span under a dialect that enables them.
    #[test]
    fn dotted_js_call_records_exact_path_provenance() {
        let source = "console.log(\"hi\");\n";
        let ir = parse_source_with_dialect(
            source,
            &DOTTED_DIALECT,
            SharedParserOptions {
                source_id: 0,
                allow_implicit_externs: false,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: false,
            },
        )
        .expect("dotted call must parse");
        let index = ir.parsed_semantic_index.as_ref().expect("index present");

        let log = index
            .call_sites
            .iter()
            .find(|site| site.name == "console.log")
            .expect("console.log site");
        assert_eq!(span_slice(source, log.callee_span), "console.log");
        assert_eq!(span_slice(source, log.expr_span), "console.log(\"hi\")");
        assert!(log.is_namespace_call, "dotted call flagged as namespace");
    }
}
