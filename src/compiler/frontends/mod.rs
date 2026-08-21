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
        parsed_semantic_index: Some(parser.take_parsed_semantic_index()),
        catalog_visibility: Some(parser.take_catalog_visibility()),
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
            parsed_semantic_index: Some(parser.take_parsed_semantic_index()),
            catalog_visibility: Some(parser.take_catalog_visibility()),
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
