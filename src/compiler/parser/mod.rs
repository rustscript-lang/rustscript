mod cursor;
mod expressions;
mod format;
mod lexer;
mod lint;
mod statements;
mod symbols;

use std::collections::{HashMap, HashSet};

use rt_format::{NoNamedArguments, ParsedFormat};

use crate::ValueType;
use crate::builtins::{
    BuiltinFunction, builtin_namespace_hint, default_host_callable, is_builtin_namespace,
    resolve_builtin_namespace_call,
};
use crate::compiler::modules::{UseDecl, UsePathSegment};
use crate::compiler::source_map::{SourceId, Span};
use crate::host_api::{HostApiCatalog, HostFunctionSchema, ResourceTypeKey};

pub(crate) use self::expressions::host_generic_type_arg_arity;
use self::lexer::{Lexer, ParserFormatArg, Token, TokenKind, is_ident_continue, is_ident_start};
use self::symbols::is_virtual_host_namespace_spec;
use super::{
    ParseError, ReplLocalBinding, STDLIB_PRINT_ARITY, STDLIB_PRINT_NAME,
    ir::{
        AssignmentKind, CatalogVisibility, ClosureExpr, Expr, FunctionDecl, FunctionDeclSite,
        FunctionImpl, FunctionParam, FunctionRefSite, FunctionRefTarget, HostApiIrMetadata,
        LexerToken, LocalDeclSite, LocalRefSite, LocalSlot, MatchPattern, MatchTypePattern,
        ModuleNamespaceAlias, ParsedCallSite, ParsedCallTarget, ParsedLexicalScope,
        ParsedSemanticIndex, ResolvedHostCall, ScopeId, SemanticNodeId, Stmt, StmtSpanSite,
        StructDecl, StructDeclSite, TypeSchema,
    },
};

pub trait ParserDialect {
    fn is_import_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn is_from_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn is_fn_alias_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn is_let_alias_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn allow_import_stmt(&self) -> bool {
        false
    }

    fn allow_return_stmt(&self) -> bool {
        false
    }

    fn allow_require_declaration(&self) -> bool {
        false
    }

    fn allow_typeof_operator(&self) -> bool {
        false
    }

    fn allow_arrow_closure(&self) -> bool {
        false
    }

    fn allow_dotted_call(&self) -> bool {
        false
    }

    fn allow_namespace_path_separator(&self) -> bool {
        true
    }

    fn allow_let_mut_binding(&self) -> bool {
        false
    }

    fn allow_macro_calls(&self) -> bool {
        false
    }

    fn allow_plus_equal_operator(&self) -> bool {
        false
    }

    fn allow_increment_operator(&self) -> bool {
        false
    }

    fn allow_parenthesized_for_loop(&self) -> bool {
        false
    }

    fn allow_for_in_loop(&self) -> bool {
        false
    }
}

pub(super) fn lint_trailing_function_return_semicolons(
    source: &str,
    source_id: SourceId,
    dialect: &'static dyn ParserDialect,
) -> Result<Vec<ParseError>, ParseError> {
    lint::lint_trailing_function_return_semicolons(source, source_id, dialect)
}

pub(super) fn format_source(
    source: &str,
    dialect: &'static dyn ParserDialect,
) -> Result<String, ParseError> {
    format::format_source(source, dialect)
}

pub(super) struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    locals: HashMap<String, LocalSlot>,
    named_local_bindings: Vec<(String, LocalSlot)>,
    next_local: LocalSlot,
    functions: HashMap<String, FunctionDecl>,
    function_list: Vec<FunctionDecl>,
    function_impls: HashMap<u16, FunctionImpl>,
    parsed_function_decls: HashSet<u16>,
    next_function: u16,
    closure_scopes: Vec<HashMap<String, LocalSlot>>,
    closure_capture_contexts: Vec<ClosureCaptureContext>,
    struct_schemas: HashMap<String, StructDecl>,
    schema_reference_sites: Vec<(String, usize, usize, Span)>,
    active_type_params: Vec<HashSet<String>>,
    unknown_type_spans: Vec<Span>,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    enforce_mutable_bindings: bool,
    dialect: &'static dyn ParserDialect,
    loop_depth: usize,
    function_body_depth: usize,
    host_namespace_aliases: HashMap<String, String>,
    direct_host_call_aliases: HashMap<String, String>,
    direct_host_wildcard_imports: HashSet<String>,
    /// Names created through the implicit-extern fallback (module mode).
    /// The source loader uses this marker to keep synthetic externs out of
    /// module declaration/export tables and to resolve (or reject) their
    /// call sites.
    implicit_extern_names: HashSet<String>,
    /// Namespace aliases introduced by file-module `use` directives.
    ///
    /// Unlike [`Parser::host_namespace_aliases`] these are recorded in every
    /// parse mode; a namespace call that is neither builtin nor host resolves
    /// through this map into a loader-resolved module call placeholder.
    module_namespace_aliases: HashMap<String, String>,
    use_declarations: Vec<UseDecl>,
    import_scan_mode: bool,
    mutable_locals: Vec<bool>,
    borrowed_map_iter_locals: Vec<LocalSlot>,
    local_schemas: HashMap<LocalSlot, TypeSchema>,
    /// Immutable host-API catalog snapshot threaded from the compile options.
    ///
    /// `Some` when a [`HostApiCatalog`] was supplied on the
    /// [`CompileSourceFileOptions`](crate::compiler::CompileSourceFileOptions)
    /// for this parse; `None` for REPL and public dialect parses, which carry
    /// no catalog. When present it is authoritative for any host name it
    /// declares.
    host_catalog: Option<std::sync::Arc<HostApiCatalog>>,
    /// Fingerprint-bound host candidate metadata produced from
    /// [`Parser::host_catalog`].
    ///
    /// `Some` exactly when a catalog is present, holding the catalog
    /// fingerprint even when the source makes zero host calls. `None` when no
    /// catalog was supplied.
    host_api_metadata: Option<HostApiIrMetadata>,
    /// Catalog-declared host function declarations, keyed by `(name, arity)`.
    ///
    /// Distinct arities of the same host name are distinct flat functions, so
    /// they are kept out of the name-only [`Parser::functions`] map (which
    /// still owns user-declared, builtin and extern identities) and tracked
    /// here by `(name, arity)` so the same overload call reuses its index
    /// without colliding across arities.
    catalog_function_decls: HashMap<(String, u8), FunctionDecl>,
    /// Parser-produced semantic provenance index tracked during parse.
    parsed_semantic_index: ParsedSemanticIndex,
    /// Parser scope stack for tracking current scope during parse.
    parser_scope_stack: Vec<ScopeId>,
}

struct ClosureCaptureContext {
    by_name: HashMap<String, LocalSlot>,
    capture_copies: Vec<(LocalSlot, LocalSlot)>,
}

impl Parser {
    pub(super) fn new(
        source: &str,
        source_id: SourceId,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
        enforce_mutable_bindings: bool,
        import_scan_mode: bool,
        dialect: &'static dyn ParserDialect,
    ) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(source, source_id, dialect);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token()?;
            let is_eof = matches!(token.kind, TokenKind::Eof);
            tokens.push(token);
            if is_eof {
                break;
            }
        }
        Ok(Self {
            tokens,
            pos: 0,
            locals: HashMap::new(),
            named_local_bindings: Vec::new(),
            next_local: 0,
            functions: HashMap::new(),
            function_list: Vec::new(),
            function_impls: HashMap::new(),
            parsed_function_decls: HashSet::new(),
            next_function: 0,
            closure_scopes: Vec::new(),
            closure_capture_contexts: Vec::new(),
            struct_schemas: HashMap::new(),
            schema_reference_sites: Vec::new(),
            active_type_params: Vec::new(),
            unknown_type_spans: Vec::new(),
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            dialect,
            loop_depth: 0,
            function_body_depth: 0,
            host_namespace_aliases: HashMap::new(),
            direct_host_call_aliases: HashMap::new(),
            direct_host_wildcard_imports: HashSet::new(),
            implicit_extern_names: HashSet::new(),
            module_namespace_aliases: HashMap::new(),
            use_declarations: Vec::new(),
            import_scan_mode,
            mutable_locals: Vec::new(),
            borrowed_map_iter_locals: Vec::new(),
            local_schemas: HashMap::new(),
            host_catalog: None,
            host_api_metadata: None,
            catalog_function_decls: HashMap::new(),
            parsed_semantic_index: ParsedSemanticIndex::default(),
            parser_scope_stack: vec![0],
        })
    }

    /// Catalog-aware constructor that additionally threads the immutable
    /// [`HostApiCatalog`] snapshot from the compile options.
    ///
    /// This is the internal entry point used by RustScript file/module parses.
    /// The frontend increments the options-held `Arc` when entering the parser
    /// boundary; this constructor consumes that `Arc` into the parser, so the
    /// catalog allocation and its data are never copied. The metadata carrier
    /// is initialized once for the parse. [`Parser::define_host_function`] may
    /// temporarily increment the `Arc` again per catalog host call to release
    /// the `self` borrow. REPL and the public [`ParserDialect`] path keep using
    /// [`Parser::new`] and thus stay catalog-free (`host_api_metadata` `None`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_host_catalog(
        source: &str,
        source_id: SourceId,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
        enforce_mutable_bindings: bool,
        import_scan_mode: bool,
        dialect: &'static dyn ParserDialect,
        catalog: std::sync::Arc<HostApiCatalog>,
    ) -> Result<Self, ParseError> {
        let mut parser = Self::new(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            import_scan_mode,
            dialect,
        )?;
        parser.host_api_metadata = Some(HostApiIrMetadata::new(catalog.fingerprint()));
        parser.host_catalog = Some(catalog);
        Ok(parser)
    }

    pub(super) fn new_with_predeclared_locals(
        source: &str,
        source_id: SourceId,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
        enforce_mutable_bindings: bool,
        dialect: &'static dyn ParserDialect,
        predeclared_locals: &[ReplLocalBinding],
    ) -> Result<Self, ParseError> {
        let parser = Self::new_with_predeclared_locals_and_host_catalog(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            dialect,
            predeclared_locals,
            None,
        )?;
        Ok(parser)
    }

    /// Catalog-aware REPL constructor: combines the predeclared-locals path
    /// with an optional [`HostApiCatalog`] snapshot so REPL compiles emit
    /// exact V13 `HostImport` schemas against the standard snapshot (when a
    /// catalog is supplied) instead of name-only imports.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_predeclared_locals_and_host_catalog(
        source: &str,
        source_id: SourceId,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
        enforce_mutable_bindings: bool,
        dialect: &'static dyn ParserDialect,
        predeclared_locals: &[ReplLocalBinding],
        host_catalog: Option<std::sync::Arc<HostApiCatalog>>,
    ) -> Result<Self, ParseError> {
        let mut parser = Self::new(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            false,
            dialect,
        )?;
        if let Some(catalog) = host_catalog {
            parser.host_api_metadata = Some(HostApiIrMetadata::new(catalog.fingerprint()));
            parser.host_catalog = Some(catalog);
        }
        for binding in predeclared_locals {
            parser.predeclare_local(binding)?;
        }
        Ok(parser)
    }

    pub(super) fn use_declarations(&self) -> Vec<UseDecl> {
        self.use_declarations.clone()
    }

    pub(super) fn parse_program(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.predeclare_functions()?;
        // Record root scope (first token to EOF). The root scope has no
        // parent; clear the sentinel so the first real scope gets `None`.
        let root_span = self
            .tokens
            .last()
            .map(|t| Span::new(t.span.source_id, 0, t.span.hi))
            .unwrap_or(Span::new(0, 0, 0));
        self.parser_scope_stack.clear();
        self.enter_scope(root_span);
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::Eof) {
            stmts.push(self.parse_stmt()?);
        }
        // Import-scan parses exist only to discover `use` directives; body
        // semantic validation (unknown struct schemas, callable contracts,
        // mutability) is deferred to the real compile parse so an unrelated
        // body error can never hide a valid import.
        if !self.import_scan_mode {
            self.validate_schema_reference_sites()?;
        }
        Ok(stmts)
    }

    fn predeclare_functions(&mut self) -> Result<(), ParseError> {
        let mut index = 0usize;
        while index < self.tokens.len() {
            match &self.tokens[index].kind {
                TokenKind::Fn => {
                    let line = self.tokens[index].line;
                    let Some(Token {
                        kind: TokenKind::Ident(name),
                        ..
                    }) = self.tokens.get(index + 1)
                    else {
                        index += 1;
                        continue;
                    };
                    let name = name.clone();
                    let exported =
                        index > 0 && matches!(self.tokens[index - 1].kind, TokenKind::Pub);
                    let mut cursor = index + 2;
                    let mut type_params = Vec::new();
                    if self
                        .tokens
                        .get(cursor)
                        .is_some_and(|token| matches!(token.kind, TokenKind::Less))
                    {
                        cursor += 1;
                        while let Some(token) = self.tokens.get(cursor) {
                            match &token.kind {
                                TokenKind::Ident(param) => type_params.push(param.clone()),
                                TokenKind::Greater => {
                                    cursor += 1;
                                    break;
                                }
                                _ => {}
                            }
                            cursor += 1;
                        }
                    }
                    if !self
                        .tokens
                        .get(cursor)
                        .is_some_and(|token| matches!(token.kind, TokenKind::LParen))
                    {
                        index += 1;
                        continue;
                    }
                    cursor += 1;
                    let mut arity = 0usize;
                    let mut angle_depth = 0usize;
                    let mut paren_depth = 1usize;
                    let mut has_param = false;
                    while let Some(token) = self.tokens.get(cursor) {
                        match token.kind {
                            TokenKind::LParen => paren_depth += 1,
                            TokenKind::RParen if paren_depth == 1 && angle_depth == 0 => {
                                if has_param {
                                    arity += 1;
                                }
                                break;
                            }
                            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
                            TokenKind::Less => angle_depth += 1,
                            TokenKind::Greater => angle_depth = angle_depth.saturating_sub(1),
                            TokenKind::Comma if paren_depth == 1 && angle_depth == 0 => {
                                if has_param {
                                    arity += 1;
                                    has_param = false;
                                }
                            }
                            _ if paren_depth == 1 && angle_depth == 0 => has_param = true,
                            _ => {}
                        }
                        cursor += 1;
                    }
                    if self.functions.contains_key(&name) {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line,
                            message: format!("duplicate function '{name}'"),
                        });
                    }
                    let arity = u8::try_from(arity).map_err(|_| ParseError {
                        span: None,
                        code: None,
                        line,
                        message: "function arity too large".to_string(),
                    })?;
                    let function_index = self.next_function;
                    self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: "function index overflow".to_string(),
                    })?;
                    let decl = FunctionDecl {
                        name: name.clone(),
                        arity,
                        index: function_index,
                        args: vec![String::new(); usize::from(arity)],
                        arg_schemas: vec![None; usize::from(arity)],
                        return_schema: None,
                        type_params,
                        exported,
                        return_type: ValueType::Unknown,
                        symbol: None,
                    };
                    self.functions.insert(name, decl.clone());
                    self.function_list.push(decl);
                    index = cursor;
                }
                _ => index += 1,
            }
        }
        Ok(())
    }

    pub(super) fn local_count(&self) -> usize {
        self.next_local as usize
    }

    pub(super) fn function_decls(&self) -> Vec<FunctionDecl> {
        self.function_list.clone()
    }

    pub(super) fn function_impls(&self) -> HashMap<u16, FunctionImpl> {
        self.function_impls.clone()
    }

    /// Cloned host candidate metadata produced by this parse.
    ///
    /// `Some` (bound to the catalog fingerprint, even with zero declared host
    /// calls) exactly when a [`HostApiCatalog`] was threaded into the parser;
    /// `None` when parse had no catalog. The carrier holds the complete
    /// candidate schema lists recorded per catalog-declared flat function.
    pub(super) fn host_api_metadata(&self) -> Option<HostApiIrMetadata> {
        self.host_api_metadata.clone()
    }

    pub(super) fn local_bindings(&self) -> Vec<(String, LocalSlot)> {
        let mut locals = self.named_local_bindings.clone();
        locals.sort_by_key(|(_, index)| *index);
        locals
    }

    pub(super) fn local_bindings_with_mutability(&self) -> Vec<ReplLocalBinding> {
        let mut locals = self
            .locals
            .iter()
            .map(|(name, index)| ReplLocalBinding {
                name: name.clone(),
                mutable: self.is_local_slot_mutable(*index),
                schema: None,
                optional: false,
            })
            .collect::<Vec<_>>();
        locals.sort_by_key(|binding| self.locals.get(&binding.name).copied().unwrap_or(0));
        locals
    }

    pub(super) fn struct_schemas(&self) -> HashMap<String, StructDecl> {
        self.struct_schemas.clone()
    }

    pub(super) fn unknown_type_spans(&self) -> Vec<Span> {
        self.unknown_type_spans.clone()
    }

    pub(super) fn implicit_extern_names(&self) -> Vec<String> {
        let mut names = self
            .implicit_extern_names
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub(super) fn is_implicit_extern(&self, name: &str) -> bool {
        self.implicit_extern_names.contains(name)
    }

    /// Take the parser's semantic provenance index.
    pub(super) fn take_parsed_semantic_index(&mut self) -> ParsedSemanticIndex {
        std::mem::take(&mut self.parsed_semantic_index)
    }

    /// Take the parser's full lexer token stream as structured metadata.
    ///
    /// The raw lexer token spans are narrowed to their exact range and
    /// translated into language-service oriented [`LexerToken`] records; the
    /// trailing EOF token is dropped. Identifiers carry their text.
    pub(super) fn take_lexer_tokens(&mut self) -> Vec<LexerToken> {
        self.tokens
            .iter()
            .filter(|token| !matches!(token.kind, TokenKind::Eof))
            .map(|token| LexerToken {
                kind: lexer_token_kind_tag(&token.kind),
                ident: match &token.kind {
                    TokenKind::Ident(name) => name.clone(),
                    _ => String::new(),
                },
                span: token.span,
            })
            .collect()
    }

    /// Take the parser's catalog visibility.
    pub(super) fn take_catalog_visibility(&mut self) -> CatalogVisibility {
        CatalogVisibility {
            host_namespace_aliases: self
                .host_namespace_aliases
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            direct_host_call_aliases: self
                .direct_host_call_aliases
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            direct_host_wildcard_imports: self
                .direct_host_wildcard_imports
                .iter()
                .cloned()
                .collect(),
            module_namespace_aliases: self
                .module_namespace_aliases
                .iter()
                .map(|(alias, module_path)| ModuleNamespaceAlias {
                    alias: alias.clone(),
                    module_path: module_path.clone(),
                    source: String::new(),
                })
                .collect(),
            use_declarations: std::mem::take(&mut self.use_declarations),
        }
    }

    /// Current scope id.
    pub(super) fn current_scope_id(&self) -> ScopeId {
        *self.parser_scope_stack.last().copied().get_or_insert(0)
    }

    /// Allocate a [`SemanticNodeId`] and record a call site in the provenance
    /// index. Returns `Some(id)` for every recorded call; the [`Option`]
    /// return type keeps the signature symmetric with node builders that may
    /// fall back to a synthetic call without a site. Every parser caller
    /// passes a real source expression and receives `Some`.
    pub(super) fn alloc_call_id(
        &mut self,
        callee_span: Span,
        expr_span: Span,
        target: ParsedCallTarget,
        name: String,
        is_namespace_call: bool,
    ) -> Option<SemanticNodeId> {
        let id = self.parsed_semantic_index.alloc_node_id();
        let scope_id = self.current_scope_id();
        self.parsed_semantic_index.call_sites.push(ParsedCallSite {
            id,
            callee_span,
            expr_span,
            target,
            name,
            scope_id,
            is_namespace_call,
        });
        Some(id)
    }

    /// Allocate provenance for a direct local-callable call
    /// (`name(...)` where `name` binds a local). Records the exact callee
    /// token span and the full call span through the closing `)`.
    pub(super) fn alloc_local_call_id(
        &mut self,
        callee_span: Span,
        rparen_span: Span,
        slot: LocalSlot,
        name: String,
    ) -> Option<SemanticNodeId> {
        let expr_span = Span::new(callee_span.source_id, callee_span.lo, rparen_span.hi);
        self.alloc_call_id(
            callee_span,
            expr_span,
            ParsedCallTarget::Local(slot),
            name,
            false,
        )
    }

    /// Build an [`Expr::Call`] with provenance tracking. Returns the call
    /// expression with the fifth field set to `Some(id)`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_call_expr_with_provenance(
        &mut self,
        index: u16,
        type_args: Vec<TypeSchema>,
        args: Vec<Expr>,
        host_resolution: Option<Box<ResolvedHostCall>>,
        callee_span: Span,
        name: String,
        is_namespace_call: bool,
    ) -> Expr {
        // Compute expr_span from callee start through the last consumed token.
        let expr_span = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| Span::new(callee_span.source_id, callee_span.lo, t.span.hi))
            .unwrap_or(callee_span);
        let semantic_id = self.alloc_call_id(
            callee_span,
            expr_span,
            ParsedCallTarget::Function(index),
            name,
            is_namespace_call,
        );
        Expr::Call(index, type_args, args, host_resolution, semantic_id)
    }

    /// Attach exact provenance to an ordinary source-level [`Expr::Call`]
    /// that was built by a direct identifier/path + `(args)` branch without
    /// its own provenance tracking.
    ///
    /// Only plain `Expr::Call(..., None)` expressions are annotated — local
    /// calls, function-value references, and compiler-synthetic calls built
    /// by helpers lacking direct source syntax pass through untouched. The
    /// `callee_span` is the exact callee token range captured before args;
    /// the recorded expr span runs from the callee start through the closing
    /// `)` (`rparen_span`).
    pub(super) fn attach_ordinary_call_provenance(
        &mut self,
        expr: Expr,
        callee_span: Span,
        rparen_span: Span,
        name: String,
    ) -> Expr {
        let Expr::Call(index, type_args, args, host_resolution, None) = expr else {
            return expr;
        };
        let expr_span = Span::new(callee_span.source_id, callee_span.lo, rparen_span.hi);
        // Record the direct function callee as a function reference site with
        // the exact identifier token span.
        self.record_func_ref(callee_span, index, name.clone());
        let semantic_id = self.alloc_call_id(
            callee_span,
            expr_span,
            ParsedCallTarget::Function(index),
            name,
            false,
        );
        Expr::Call(index, type_args, args, host_resolution, semantic_id)
    }

    /// Attach exact provenance to a builtin/host namespace call
    /// (`json::encode(...)`, `math::abs(...)`) or a dotted JS call
    /// (`console.log(...)`) that was built by a path-based branch without its
    /// own provenance tracking.
    ///
    /// Only plain `Expr::Call(..., None)` expressions are annotated. The
    /// `callee_span` is the exact full namespace path token range
    /// (`json::encode`); the recorded expr span runs from the path start
    /// through the closing `)` of the consumed argument list. The call is
    /// marked as a namespace call so downstream consumers can distinguish
    /// path-based calls from plain direct calls.
    pub(super) fn attach_namespace_call_provenance(
        &mut self,
        expr: Expr,
        callee_span: Span,
        name: String,
    ) -> Expr {
        let Expr::Call(index, type_args, args, host_resolution, None) = expr else {
            return expr;
        };
        let expr_span = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| Span::new(callee_span.source_id, callee_span.lo, t.span.hi))
            .unwrap_or(callee_span);
        let semantic_id = self.alloc_call_id(
            callee_span,
            expr_span,
            ParsedCallTarget::Function(index),
            name,
            true,
        );
        Expr::Call(index, type_args, args, host_resolution, semantic_id)
    }

    /// Record a local declaration site.
    pub(super) fn record_local_decl(
        &mut self,
        ident_span: Span,
        stmt_span: Span,
        slot: LocalSlot,
        name: String,
    ) {
        let id = self.parsed_semantic_index.alloc_node_id();
        let scope_id = self.current_scope_id();
        let decl_order =
            if let Some(scope) = self.parsed_semantic_index.scopes.get_mut(scope_id as usize) {
                let order = scope.declarations.len() as u32;
                scope.declarations.push(slot);
                order
            } else {
                0
            };
        self.parsed_semantic_index.local_decls.push(LocalDeclSite {
            id,
            ident_span,
            stmt_span,
            slot,
            name,
            scope_id,
            decl_order,
        });
    }

    /// Record a local variable reference site.
    pub(super) fn record_local_ref(&mut self, ident_span: Span, slot: LocalSlot, name: String) {
        let id = self.parsed_semantic_index.alloc_node_id();
        let scope_id = self.current_scope_id();
        self.parsed_semantic_index.local_refs.push(LocalRefSite {
            id,
            ident_span,
            slot,
            name,
            scope_id,
        });
    }

    /// Record a function declaration site.
    pub(super) fn record_func_decl(&mut self, ident_span: Span, function_index: u16, name: String) {
        let id = self.parsed_semantic_index.alloc_node_id();
        let scope_id = self.current_scope_id();
        let decl_order =
            if let Some(scope) = self.parsed_semantic_index.scopes.get_mut(scope_id as usize) {
                let order = scope.functions.len() as u32;
                scope.functions.push(function_index);
                order
            } else {
                0
            };
        self.parsed_semantic_index
            .func_decls
            .push(FunctionDeclSite {
                id,
                ident_span,
                function_index,
                name,
                scope_id,
                decl_order,
            });
    }

    /// Record a function value reference site.
    pub(super) fn record_func_ref(&mut self, ident_span: Span, function_index: u16, name: String) {
        self.record_func_ref_target(
            ident_span,
            FunctionRefTarget::Function(function_index),
            name,
        );
    }

    /// Record a struct declaration site.
    ///
    /// Structs have no flat function index, so the provenance site carries
    /// the exact identifier span, the full `struct`..`}` declaration span,
    /// and the declaring scope. Strict-mode resolution uses the declaration
    /// span to point at the exact struct declaration in diagnostics.
    pub(super) fn record_struct_decl(&mut self, ident_span: Span, decl_span: Span, name: String) {
        let id = self.parsed_semantic_index.alloc_node_id();
        let scope_id = self.current_scope_id();
        self.parsed_semantic_index
            .struct_decls
            .push(StructDeclSite {
                id,
                ident_span,
                decl_span,
                name,
                scope_id,
            });
    }

    pub(super) fn record_func_ref_target(
        &mut self,
        ident_span: Span,
        target: FunctionRefTarget,
        name: String,
    ) {
        let id = self.parsed_semantic_index.alloc_node_id();
        let scope_id = self.current_scope_id();
        self.parsed_semantic_index.func_refs.push(FunctionRefSite {
            id,
            ident_span,
            target,
            name,
            scope_id,
        });
    }

    /// Enter a new scope and return its id.
    pub(super) fn enter_scope(&mut self, range: Span) -> ScopeId {
        let id = self.parsed_semantic_index.alloc_scope_id();
        let parent = self.parser_scope_stack.last().copied();
        self.parsed_semantic_index.scopes.push(ParsedLexicalScope {
            id,
            parent,
            range,
            declarations: Vec::new(),
            functions: Vec::new(),
        });
        self.parser_scope_stack.push(id);
        id
    }

    /// Exit the current scope.
    pub(super) fn exit_scope(&mut self) {
        self.parser_scope_stack.pop();
    }

    /// Run `f` inside a fresh child scope and exit on every path (success or
    /// error), keeping the parser scope stack balanced. The scope's recorded
    /// range spans `open_span.lo` through the last token consumed by `f` (the
    /// closing `}` of a brace block, or the final token of an expression
    /// production). Returns the scope id so callers can assert on it.
    pub(super) fn with_scope<T>(
        &mut self,
        open_span: Span,
        f: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        let scope_id = self.enter_scope(open_span);
        let result = f(self);
        let close_hi = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|token| token.span.hi)
            .unwrap_or(open_span.hi);
        if let Some(scope) = self.parsed_semantic_index.scopes.get_mut(scope_id as usize) {
            scope.range.hi = close_hi.max(open_span.hi);
        }
        self.exit_scope();
        result
    }

    /// Look up a file-module namespace alias recorded from a structured
    /// `use` directive (both parse modes).
    pub(super) fn module_namespace_alias(&self, namespace: &str) -> Option<&str> {
        self.module_namespace_aliases
            .get(namespace)
            .map(String::as_str)
    }

    fn validate_schema_reference_sites(&self) -> Result<(), ParseError> {
        for (name, arg_count, line, span) in &self.schema_reference_sites {
            let Some(decl) = self.struct_schemas.get(name) else {
                return Err(ParseError {
                    span: Some(*span),
                    code: None,
                    line: *line,
                    message: format!("unknown struct schema '{name}'"),
                });
            };
            if decl.type_params.len() != *arg_count {
                return Err(ParseError {
                    span: Some(*span),
                    code: None,
                    line: *line,
                    message: format!(
                        "struct schema '{name}' expects {} type arguments, got {}",
                        decl.type_params.len(),
                        arg_count
                    ),
                });
            }
            if self.struct_schemas.contains_key(name) {
                continue;
            }
        }
        Ok(())
    }

    fn push_active_type_params(&mut self, params: &[String]) {
        self.active_type_params
            .push(params.iter().cloned().collect::<HashSet<_>>());
    }

    fn pop_active_type_params(&mut self) {
        self.active_type_params.pop();
    }

    fn is_active_type_param(&self, name: &str) -> bool {
        self.active_type_params
            .iter()
            .rev()
            .any(|params| params.contains(name))
    }

    fn parse_type_params(
        &mut self,
        owner: &str,
        owner_name: &str,
    ) -> Result<Vec<String>, ParseError> {
        if !self.check(&TokenKind::Less) {
            return Ok(Vec::new());
        }

        self.expect(&TokenKind::Less, "expected '<' before type parameters")?;
        let mut params = Vec::new();
        let mut seen = HashSet::new();
        loop {
            let param = self.expect_ident("expected type parameter name")?;
            if !seen.insert(param.clone()) {
                return Err(ParseError {
                    span: Some(self.current_span()),
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "duplicate type parameter '{param}' in {owner} '{owner_name}'"
                    ),
                });
            }
            params.push(param);
            if self.match_kind(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.expect(&TokenKind::Greater, "expected '>' after type parameters")?;
        Ok(params)
    }

    fn parse_turbofish_type_args(&mut self) -> Result<Vec<TypeSchema>, ParseError> {
        if !self.check_path_separator() || !self.check_kind_at(self.pos + 2, &TokenKind::Less) {
            return Ok(Vec::new());
        }

        self.match_path_separator();
        self.expect(&TokenKind::Less, "expected '<' after '::' in turbofish")?;
        let mut type_args = Vec::new();
        loop {
            type_args.push(self.parse_declared_type_schema()?);
            if self.match_kind(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.expect(&TokenKind::Greater, "expected '>' after type arguments")?;
        Ok(type_args)
    }

    fn function_param_names(params: &[FunctionParam]) -> Vec<String> {
        params.iter().map(|param| param.name.clone()).collect()
    }
}

/// A stable string tag for a lexer token kind, used by the language-service
/// token metadata. The tag is the [`TokenKind`] variant name; identifier
/// tokens keep the `Ident` tag with their text carried separately.
fn lexer_token_kind_tag(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Ident(_) => "Ident".to_string(),
        TokenKind::Int(_) => "Int".to_string(),
        TokenKind::IntMinMagnitude(_) => "IntMinMagnitude".to_string(),
        TokenKind::Float(_) => "Float".to_string(),
        TokenKind::String(_) => "String".to_string(),
        TokenKind::Bytes(_) => "Bytes".to_string(),
        TokenKind::True => "True".to_string(),
        TokenKind::False => "False".to_string(),
        TokenKind::Null => "Null".to_string(),
        TokenKind::Pub => "Pub".to_string(),
        TokenKind::Use => "Use".to_string(),
        TokenKind::Import => "Import".to_string(),
        TokenKind::From => "From".to_string(),
        TokenKind::As => "As".to_string(),
        TokenKind::Fn => "Fn".to_string(),
        TokenKind::Struct => "Struct".to_string(),
        TokenKind::Let => "Let".to_string(),
        TokenKind::For => "For".to_string(),
        TokenKind::If => "If".to_string(),
        TokenKind::Else => "Else".to_string(),
        TokenKind::Match => "Match".to_string(),
        TokenKind::While => "While".to_string(),
        TokenKind::Break => "Break".to_string(),
        TokenKind::Continue => "Continue".to_string(),
        TokenKind::Bang => "Bang".to_string(),
        TokenKind::BangEqual => "BangEqual".to_string(),
        TokenKind::Plus => "Plus".to_string(),
        TokenKind::PlusPlus => "PlusPlus".to_string(),
        TokenKind::PlusEqual => "PlusEqual".to_string(),
        TokenKind::Minus => "Minus".to_string(),
        TokenKind::Star => "Star".to_string(),
        TokenKind::Slash => "Slash".to_string(),
        TokenKind::Percent => "Percent".to_string(),
        TokenKind::Ampersand => "Ampersand".to_string(),
        TokenKind::AmpersandAmpersand => "AmpersandAmpersand".to_string(),
        TokenKind::PipePipe => "PipePipe".to_string(),
        TokenKind::Pipe => "Pipe".to_string(),
        TokenKind::LParen => "LParen".to_string(),
        TokenKind::RParen => "RParen".to_string(),
        TokenKind::LBracket => "LBracket".to_string(),
        TokenKind::RBracket => "RBracket".to_string(),
        TokenKind::LBrace => "LBrace".to_string(),
        TokenKind::RBrace => "RBrace".to_string(),
        TokenKind::Comma => "Comma".to_string(),
        TokenKind::Colon => "Colon".to_string(),
        TokenKind::Question => "Question".to_string(),
        TokenKind::Dot => "Dot".to_string(),
        TokenKind::DotDot => "DotDot".to_string(),
        TokenKind::DotDotEqual => "DotDotEqual".to_string(),
        TokenKind::Ellipsis => "Ellipsis".to_string(),
        TokenKind::Semicolon => "Semicolon".to_string(),
        TokenKind::Equal => "Equal".to_string(),
        TokenKind::EqualEqual => "EqualEqual".to_string(),
        TokenKind::FatArrow => "FatArrow".to_string(),
        TokenKind::Less => "Less".to_string(),
        TokenKind::LessEqual => "LessEqual".to_string(),
        TokenKind::Greater => "Greater".to_string(),
        TokenKind::GreaterEqual => "GreaterEqual".to_string(),
        TokenKind::Eof => "Eof".to_string(),
    }
}
