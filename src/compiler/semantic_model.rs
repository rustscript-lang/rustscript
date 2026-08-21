//! Host-agnostic semantic model for language-service queries.
//!
//! This module owns the reusable query surface that editors and LSP adapters
//! consume: hover (inferred type schema), signature help (resolved host call
//! signature), in-editor diagnostics, completions (visible symbols + catalog
//! candidates), and go-to-definition (virtual host declaration).
//!
//! ## Design invariants
//!
//! * **Single compilation pass.** [`SemanticModel`] is produced from the *same*
//!   [`FrontendIr`] that the compiler's [`crate::compiler::pipeline`] legalizes
//!   and type-checks. No second parser, type engine, name-only lookup, or
//!   hardcoded builtin resource table is used.
//! * **Exact catalog snapshot.** The model carries the same
//!   [`Arc<HostApiCatalog>`] snapshot (and its [`HostApiFingerprint`]) that
//!   [`CompileSourceFileOptions`] received. The catalog fingerprint is exposed
//!   as a read-only accessor.
//! * **Per-call resolution.** The [`Expr::Call`] nodes carry
//!   [`ResolvedHostCall`] annotations with the exact per-call
//!   [`HostFunctionSchema`] (name, parameter schemas, passing modes, return
//!   schema, catalog fingerprint). Signature help and hover read these
//!   annotations; they never reconstruct resolution from the index.
//! * **Resource types are nominal.** Inferred schemas for host results show
//!   `resource<qualified.key>` (e.g. `resource<sqlite.connection>`). Wrong
//!   resource diagnostics include expected and actual keys plus the source span.
//! * **Deterministic position queries.** All position queries resolve the
//!   smallest containing semantic item deterministically using the
//!   [`SemanticIndex`] sidecar. UTF-8 byte offsets with line/column semantics
//!   are documented for LSP conversion.
//! * **Standard and custom catalogs.** The public API accepts any
//!   [`Arc<HostApiCatalog>`] — standard builtin catalogs and embedding-supplied
//!   custom catalogs both work identically.
//! * **No bytecode generation.** Semantic diagnostics include compiler errors
//!   relevant to typing and host resolution; they are available without
//!   generating or running bytecode.
//!
//! ## Position semantics
//!
//! [`SourcePosition`] uses UTF-8 byte offsets within a [`SourceId`]'s source
//! text. The LSP adapter converts between LSP `Position` (0-indexed line and
//! UTF-16 code-unit column) and [`SourcePosition`] using the [`SourceMap`]'s
//! [`SourceFile::line_col_for_offset`] / [`SourceFile::line_col_to_offset`]
//! methods. The byte offset is the raw offset into the source text string
//! (`&str`), which is UTF-8. LSP clients that use UTF-16 code units for
//! columns must convert through the source text.

use std::sync::Arc;

use crate::host_api::{
    HostApiCatalog, HostApiFingerprint, HostFunctionSchema, HostParamPassing, HostTypeSchema,
};

use super::CompileError;
use super::ir::{
    CatalogVisibility, FrontendIr, FunctionRefTarget, LocalSlot, ParsedCallTarget,
    ResolvedHostCall, ScopeId, SemanticIndex, TypeSchema,
};
use super::source_map::{SourceId, SourceMap, Span};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A position in source code, expressed as a UTF-8 byte offset within a
/// [`SourceId`]'s text.
///
/// The offset is a raw byte index into the source text string (`&str`). For
/// LSP adapters, convert between LSP `Position` (line, UTF-16 code-unit
/// column) and this offset via [`SourceMap::line_col_for_offset`] and
/// [`SourceMap::line_col_to_offset`] — both operate on UTF-8 byte offsets
/// (not UTF-16) and return 1-indexed line/column values.
///
/// # LSP conversion notes
///
/// - LSP line numbers are 0-indexed; this crate's line/column helpers are
///   1-indexed. Subtract 1 from the line before sending to LSP.
/// - LSP column offsets are UTF-16 code-unit offsets. For ASCII-only source
///   text, the UTF-8 byte offset and the UTF-16 code-unit offset are the same.
///   For non-ASCII text (multi-byte UTF-8 characters), convert by counting
///   UTF-16 code units up to the byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    /// The source file this position refers to.
    pub source_id: SourceId,
    /// UTF-8 byte offset into the source text.
    pub offset: usize,
}

impl SourcePosition {
    /// Create a source position from a source ID and a UTF-8 byte offset.
    pub fn new(source_id: SourceId, offset: usize) -> Self {
        Self { source_id, offset }
    }

    /// Create a source position from a span's start.
    pub fn from_span_start(span: Span) -> Self {
        Self {
            source_id: span.source_id,
            offset: span.lo,
        }
    }

    /// Create a source position from a span's end.
    pub fn from_span_end(span: Span) -> Self {
        Self {
            source_id: span.source_id,
            offset: span.hi,
        }
    }
}

/// A semantic diagnostic produced during compilation.
///
/// These include typing errors, host-call resolution failures, and any other
/// compiler error relevant to the language-service experience. They are
/// available without generating or running bytecode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticDiagnostic {
    /// The message describing the error.
    pub message: String,
    /// The source span where the error occurred, if available.
    pub span: Option<Span>,
    /// An optional error code for IDE categorisation.
    pub code: Option<String>,
}

/// A completion item for the language service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticCompletion {
    /// The label shown in the completion list.
    pub label: String,
    /// Optional detail text (e.g. type signature).
    pub detail: Option<String>,
    /// Optional documentation string.
    pub docs: Option<String>,
    /// The kind of completion item (e.g. "function", "variable", "resource").
    pub kind: CompletionItemKind,
}

/// The kind of a completion item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionItemKind {
    /// A local variable or parameter.
    Variable,
    /// A host function.
    Function,
    /// A resource type.
    Resource,
    /// A keyword or builtin construct.
    Keyword,
}

/// A definition location for go-to-definition support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// The source span of the definition.
    pub span: Span,
    /// A human-readable label for the definition.
    pub label: String,
}

// ---------------------------------------------------------------------------
// SemanticModel
// ---------------------------------------------------------------------------

/// The language-service query surface for a single compilation unit.
///
/// Constructed from the compiled [`FrontendIr`] (after legalization and type
/// checking), the [`SourceMap`] for position resolution, and the exact
/// [`Arc<HostApiCatalog>`] snapshot used during compilation.
///
/// All position-based queries use [`SourcePosition`] and resolve the smallest
/// containing semantic item deterministically.
pub struct SemanticModel {
    /// The compiled IR, after legalization and type checking.
    ir: FrontendIr,
    /// Source map for position resolution.
    sources: SourceMap,
    /// The exact host API catalog snapshot used during compilation.
    catalog: Arc<HostApiCatalog>,
    /// Compile errors encountered during compilation.
    errors: Vec<CompileError>,
    /// The semantic index built during pipeline compilation.
    semantic_index: Option<SemanticIndex>,
}

impl SemanticModel {
    /// Build a semantic model from the compilation results.
    ///
    /// `ir` must be the fully legalized and type-checked IR. `errors` may
    /// contain typing and host-resolution errors; they are surfaced via
    /// [`Self::diagnostics`].
    pub fn new(
        ir: FrontendIr,
        sources: SourceMap,
        catalog: Arc<HostApiCatalog>,
        errors: Vec<CompileError>,
    ) -> Self {
        let semantic_index = ir.semantic_index.clone();
        Self {
            ir,
            sources,
            catalog,
            errors,
            semantic_index,
        }
    }

    // ------------------------------------------------------------------
    // Read-only accessors
    // ------------------------------------------------------------------

    /// The catalog fingerprint this model was compiled against.
    pub fn catalog_fingerprint(&self) -> HostApiFingerprint {
        self.catalog.fingerprint()
    }

    /// The underlying host API catalog snapshot.
    pub fn catalog(&self) -> &Arc<HostApiCatalog> {
        &self.catalog
    }

    /// The source map for position resolution.
    pub fn sources(&self) -> &SourceMap {
        &self.sources
    }

    /// The compiled IR.
    pub fn ir(&self) -> &FrontendIr {
        &self.ir
    }

    // ------------------------------------------------------------------
    // Hover: inferred schema at a position
    // ------------------------------------------------------------------

    /// Returns the inferred type schema at the given source position.
    ///
    /// This is the primary hover query: for a local variable binding, it
    /// returns the schema inferred by the type checker (which may be
    /// `resource<sqlite.connection>` etc.). For a call expression, it returns
    /// the call's resolved return schema. For a literal, it returns the
    /// literal's type.
    ///
    /// Returns `None` when no semantic item is found at the position.
    pub fn inferred_schema_at(&self, position: SourcePosition) -> Option<TypeSchema> {
        self.inferred_schema_at_inner(position)
    }

    fn inferred_schema_at_inner(&self, position: SourcePosition) -> Option<TypeSchema> {
        let index = self.semantic_index.as_ref()?;

        // A position on the callee identifier of a containing call resolves to
        // the call's return schema (hover on a call callee returns the call
        // schema), never to the callee symbol's own type. Positions in the
        // argument region are NOT the callee and must resolve to the exact
        // local/function identifier spans below.
        let is_call_callee = self
            .smallest_call_at(position)
            .map(|info| self.position_in_span(position, info.site.callee_span))
            .unwrap_or(false);

        if !is_call_callee {
            // 1. Local declaration or reference exact identifier span. This
            //    beats a containing call expression span: a local reference
            //    used as a call argument (`let a = 1; tag(a)`) must resolve
            //    to the local's own schema, never the call's return type.
            if let Some(slot) = self.local_slot_containing(position) {
                return index.slot_schema(slot).cloned();
            }

            // 2. Function declaration exact identifier span.
            if let Some(schema) = self.function_decl_return_at(position) {
                return Some(schema);
            }

            // 2b. Function-value reference exact identifier span: resolve the
            //     referenced function's callable signature (params -> result),
            //     never a name-only fallback.
            if let Some(schema) = self.function_ref_schema_at(position) {
                return Some(schema);
            }
        }

        // 3. Smallest containing call site (exact parser callee/expr span):
        //    return the resolved return schema.
        if let Some(schema) = self.smallest_call_return_at(position) {
            return Some(schema);
        }

        None
    }

    /// The resolved return schema of the smallest containing call site, using
    /// exact parser-origin exp/callee spans. Empty/zero-length spans never
    /// match so the position is not spuriously claimed.
    fn smallest_call_return_at(&self, position: SourcePosition) -> Option<TypeSchema> {
        let info = self.smallest_call_at(position)?;
        Some(info.return_type.clone())
    }

    /// The smallest containing [`ResolvedCallInfo`] at `position`, using only
    /// the parser-recorded callee/expr spans. Ties resolve deterministically by
    /// the shorter expression span, then the earlier start offset.
    fn smallest_call_at(
        &self,
        position: SourcePosition,
    ) -> Option<&crate::compiler::ir::ResolvedCallInfo> {
        let index = self.semantic_index.as_ref()?;
        let mut best: Option<&crate::compiler::ir::ResolvedCallInfo> = None;
        for info in index.resolved_calls.values() {
            let site = &info.site;
            if !self.position_in_span(position, site.callee_span)
                && !self.position_in_span(position, site.expr_span)
            {
                continue;
            }
            let better = match best {
                None => true,
                Some(cur) => {
                    let cur_len = cur.site.expr_span.hi - cur.site.expr_span.lo;
                    let new_len = site.expr_span.hi - site.expr_span.lo;
                    // Smaller containing span wins; ties by earlier start.
                    new_len < cur_len
                        || (new_len == cur_len && site.expr_span.lo < cur.site.expr_span.lo)
                }
            };
            if better {
                best = Some(info);
            }
        }
        best
    }

    /// The local slot whose declaration or a reference exact identifier span
    /// contains the position. Exact parser token spans only.
    fn local_slot_containing(&self, position: SourcePosition) -> Option<LocalSlot> {
        let index = self.semantic_index.as_ref()?;
        let parsed = &index.parsed;

        // Smallest containing exact identifier span wins; ties by earlier lo.
        let mut best: Option<(Option<ScopeId>, LocalSlot, Span)> = None;
        for reference in &parsed.local_refs {
            if self.position_in_span(position, reference.ident_span) {
                let candidate: (Option<ScopeId>, LocalSlot, Span) =
                    (None, reference.slot, reference.ident_span);
                best = Some(pick_smaller_span(&best, &candidate).clone());
            }
        }
        for decl in &parsed.local_decls {
            if self.position_in_span(position, decl.ident_span) {
                let candidate: (Option<ScopeId>, LocalSlot, Span) =
                    (Some(decl.scope_id), decl.slot, decl.ident_span);
                best = Some(pick_smaller_span(&best, &candidate).clone());
            }
        }
        best.map(|(_, slot, _)| slot)
    }

    /// Infer a local slot's schema by resolving a referencing decl through the
    /// parser's scope chain when multiple declarations share a slot.
    fn function_decl_return_at(&self, position: SourcePosition) -> Option<TypeSchema> {
        let index = self.semantic_index.as_ref()?;
        for decl in &index.parsed.func_decls {
            if self.position_in_span(position, decl.ident_span) {
                return index
                    .function_return_schemas
                    .get(&decl.function_index)
                    .cloned()
                    .flatten()
                    .or(Some(TypeSchema::Unknown));
            }
        }
        None
    }

    /// The callable signature schema of a function-value reference at
    /// `position` (e.g. `let f = helper;` hovering `helper`). Resolves the
    /// reference's target (flat function index or module symbol) to its
    /// declaration in the flat table — never a name-only fallback — and
    /// builds the `Callable { params, result }` schema from the declared
    /// parameter and return schemas. `None` when the position is not a
    /// function-value reference.
    fn function_ref_schema_at(&self, position: SourcePosition) -> Option<TypeSchema> {
        let index = self.semantic_index.as_ref()?;
        for reference in &index.parsed.func_refs {
            if !self.position_in_span(position, reference.ident_span) {
                continue;
            }
            let function_index = match reference.target {
                FunctionRefTarget::Function(index) => index,
                FunctionRefTarget::Module(symbol) => self
                    .ir
                    .functions
                    .iter()
                    .find(|decl| decl.symbol == Some(symbol))
                    .map(|decl| decl.index)?,
            };
            let decl = self
                .ir
                .functions
                .iter()
                .find(|decl| decl.index == function_index)?;
            let params = decl
                .arg_schemas
                .iter()
                .enumerate()
                .map(|(i, schema)| {
                    schema.clone().unwrap_or_else(|| {
                        decl.args
                            .get(i)
                            .map(|_| TypeSchema::Unknown)
                            .unwrap_or(TypeSchema::Unknown)
                    })
                })
                .collect::<Vec<_>>();
            let result = decl.return_schema.clone().unwrap_or(TypeSchema::Unknown);
            return Some(TypeSchema::Callable {
                params,
                result: Box::new(result),
            });
        }
        None
    }

    /// The declaration for a local slot that is visible from `from_scope`,
    /// resolving shadowing through the parser's lexical scope chain. Returns
    /// the deepest declaration whose scope is an ancestor (or equal) of
    /// `from_scope`, deterministically.
    fn local_decl_visible_from(
        &self,
        slot: LocalSlot,
        from_scope: ScopeId,
    ) -> Option<crate::compiler::ir::LocalDeclSite> {
        let index = self.semantic_index.as_ref()?;
        let parsed = &index.parsed;
        // Collect ancestor scope ids of `from_scope` (including itself).
        let mut ancestors = Vec::new();
        let mut current = Some(from_scope);
        let mut seen = std::collections::HashSet::new();
        while let Some(scope_id) = current {
            if !seen.insert(scope_id) {
                break;
            }
            ancestors.push(scope_id);
            current = parsed
                .scopes
                .get(scope_id as usize)
                .and_then(|scope| scope.parent);
        }
        // Among declarations for `slot`, pick the one whose scope is deepest in
        // `ancestors` (closest to `from_scope`). Ties by smallest decl_order.
        let mut best: Option<crate::compiler::ir::LocalDeclSite> = None;
        for decl in &parsed.local_decls {
            if decl.slot != slot {
                continue;
            }
            if let Some(depth) = ancestors.iter().position(|&s| s == decl.scope_id) {
                let better = match &best {
                    None => true,
                    Some(cur) => {
                        let cur_depth = ancestors
                            .iter()
                            .position(|&s| s == cur.scope_id)
                            .unwrap_or(usize::MAX);
                        depth < cur_depth
                            || (depth == cur_depth && decl.decl_order < cur.decl_order)
                    }
                };
                if better {
                    best = Some(decl.clone());
                }
            }
        }
        best
    }

    /// The exact declaration span for a function index, resolving through the
    /// parser scope chain from `from_scope` so shadowing declarations resolve
    /// to the innermost visible one (no name search).
    fn function_decl_visible_from(
        &self,
        function_index: u16,
        from_scope: ScopeId,
    ) -> Option<crate::compiler::ir::FunctionDeclSite> {
        let index = self.semantic_index.as_ref()?;
        let parsed = &index.parsed;
        let mut ancestors = Vec::new();
        let mut current = Some(from_scope);
        let mut seen = std::collections::HashSet::new();
        while let Some(scope_id) = current {
            if !seen.insert(scope_id) {
                break;
            }
            ancestors.push(scope_id);
            current = parsed
                .scopes
                .get(scope_id as usize)
                .and_then(|scope| scope.parent);
        }
        let mut best: Option<crate::compiler::ir::FunctionDeclSite> = None;
        for decl in &parsed.func_decls {
            if decl.function_index != function_index {
                continue;
            }
            if let Some(depth) = ancestors.iter().position(|&s| s == decl.scope_id) {
                let better = match &best {
                    None => true,
                    Some(cur) => {
                        let cur_depth = ancestors
                            .iter()
                            .position(|&s| s == cur.scope_id)
                            .unwrap_or(usize::MAX);
                        depth < cur_depth
                            || (depth == cur_depth && decl.decl_order < cur.decl_order)
                    }
                };
                if better {
                    best = Some(decl.clone());
                }
            }
        }
        best
    }

    // ------------------------------------------------------------------
    // Signature help: resolved host call signature at a position
    // ------------------------------------------------------------------

    /// Returns the resolved host function schema at the given position.
    ///
    /// This is the primary signature-help query: if the position falls within
    /// a call expression that was resolved against the host API catalog, the
    /// full [`HostFunctionSchema`] (name, parameter schemas with passing modes,
    /// return schema) is returned. The caller can use the parameter count to
    /// determine which parameter the cursor is on.
    ///
    /// Returns `None` when the position is not within a catalog-resolved call.
    pub fn callable_signature_at(&self, position: SourcePosition) -> Option<HostFunctionSchema> {
        let info = self.smallest_call_at(position)?;
        let resolved = info.host.as_ref()?;
        Some(self.resolved_call_to_host_schema(resolved))
    }

    /// Convert a [`ResolvedHostCall`] back into a [`HostFunctionSchema`] for
    /// signature-help display.
    fn resolved_call_to_host_schema(&self, resolved: &ResolvedHostCall) -> HostFunctionSchema {
        let params = resolved
            .params
            .iter()
            .zip(resolved.passing.iter())
            .map(|(param, passing)| crate::host_api::HostParamSchema {
                name: param.name.clone(),
                ty: self.compiler_schema_to_host_schema(&param.schema),
                passing: *passing,
            })
            .collect();

        // Look up the description from the catalog.
        let description = self
            .catalog
            .functions()
            .iter()
            .find(|f| f.name == resolved.name)
            .map(|f| f.description.clone())
            .unwrap_or_default();

        crate::host_api::HostFunctionSchema {
            name: resolved.name.clone(),
            params,
            return_type: self.compiler_schema_to_host_schema(&resolved.return_type),
            description,
        }
    }

    /// Convert a compiler [`TypeSchema`] to a [`HostTypeSchema`] for display.
    fn compiler_schema_to_host_schema(&self, schema: &TypeSchema) -> HostTypeSchema {
        match schema {
            TypeSchema::Unknown => HostTypeSchema::Unknown,
            TypeSchema::Null => HostTypeSchema::Null,
            TypeSchema::Int => HostTypeSchema::Int,
            TypeSchema::Float => HostTypeSchema::Float,
            TypeSchema::Number => HostTypeSchema::Number,
            TypeSchema::Bool => HostTypeSchema::Bool,
            TypeSchema::String => HostTypeSchema::String,
            TypeSchema::Bytes => HostTypeSchema::Bytes,
            TypeSchema::Array(inner) => {
                HostTypeSchema::Array(Box::new(self.compiler_schema_to_host_schema(inner)))
            }
            TypeSchema::Map(inner) => {
                HostTypeSchema::Map(Box::new(self.compiler_schema_to_host_schema(inner)))
            }
            TypeSchema::Optional(inner) => {
                HostTypeSchema::Optional(Box::new(self.compiler_schema_to_host_schema(inner)))
            }
            TypeSchema::Callable { params, result } => HostTypeSchema::Callable {
                params: params
                    .iter()
                    .map(|p| self.compiler_schema_to_host_schema(p))
                    .collect(),
                result: Box::new(self.compiler_schema_to_host_schema(result)),
            },
            TypeSchema::Resource(key) => HostTypeSchema::Resource(key.clone()),
            TypeSchema::Named(_name, _type_args) => HostTypeSchema::Unknown,
            TypeSchema::GenericParam(_name) => HostTypeSchema::Unknown,
            TypeSchema::ArrayTuple(_items) => {
                HostTypeSchema::Array(Box::new(HostTypeSchema::Unknown))
            }
            TypeSchema::ArrayTupleRest { prefix: _, rest: _ } => {
                HostTypeSchema::Array(Box::new(HostTypeSchema::Unknown))
            }
            TypeSchema::Object(_) => HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
        }
    }

    // ------------------------------------------------------------------
    // Diagnostics
    // ------------------------------------------------------------------

    /// Returns all semantic diagnostics from compilation.
    ///
    /// These include typing errors, host-call resolution errors, and any other
    /// compiler errors relevant to the editor experience. They are available
    /// without generating or running bytecode.
    ///
    /// Diagnostics carry exact spans where available (from `CompileError`
    /// variants that carry line + source_name), and stable error codes.
    pub fn diagnostics(&self) -> Vec<SemanticDiagnostic> {
        let mut diags = Vec::new();

        // Convert compile errors to semantic diagnostics.
        for err in &self.errors {
            let span = self.compile_error_to_span(err);
            let code = self.compile_error_to_code(err);
            diags.push(SemanticDiagnostic {
                message: err.diagnostic_message(),
                span,
                code,
            });
        }

        diags
    }

    /// Convert a `CompileError` to an optional source span.
    ///
    /// Map a `CompileError` to its exact original-source span.
    ///
    /// Every span-capable variant carries the exact parser-origin span of the
    /// failing construct, captured at the point of production and resolved
    /// from parser provenance (call/optional access SemanticNodeId -> parsed
    /// call-site span, statement line -> parsed statement span, function
    /// index -> parsed declaration identifier span). These are returned
    /// verbatim. Variants without a carried span (synthetic/test errors that
    /// genuinely carry no position, or non-positioned errors such as
    /// `CallArityOverflow`) return `None`. No source text is ever scanned and
    /// no same-line token guessing is performed.
    fn compile_error_to_span(&self, err: &CompileError) -> Option<Span> {
        let carried = match err {
            CompileError::HostCallResolve { span, .. }
            | CompileError::IfElseBranchTypeMismatch { span, .. }
            | CompileError::CallableArgumentTypeMismatch { span, .. }
            | CompileError::BinaryOperandTypeMismatch { span, .. }
            | CompileError::InvalidFieldAccess { span, .. }
            | CompileError::FunctionParameterTypeConflict { span, .. }
            | CompileError::StrictTypingRequired { span, .. } => *span,
            _ => return None,
        };
        carried
    }

    /// Map a `CompileError` to a stable error code.
    fn compile_error_to_code(&self, err: &CompileError) -> Option<String> {
        Some(match err {
            CompileError::HostCallResolve { .. } => "E001".to_string(),
            CompileError::CallArityOverflow => "E002".to_string(),
            CompileError::CallableArgumentTypeMismatch { .. } => "E003".to_string(),
            CompileError::BinaryOperandTypeMismatch { .. } => "E004".to_string(),
            CompileError::IfElseBranchTypeMismatch { .. } => "E005".to_string(),
            CompileError::InvalidFieldAccess { .. } => "E006".to_string(),
            CompileError::FunctionParameterTypeConflict { .. } => "E007".to_string(),
            CompileError::StrictTypingRequired { .. } => "E008".to_string(),
            CompileError::BreakOutsideLoop => "E009".to_string(),
            CompileError::ContinueOutsideLoop => "E010".to_string(),
            CompileError::Assembler(_) => "E011".to_string(),
            CompileError::HostImportOverflow => "E012".to_string(),
            CompileError::ClosureUsedAsValue => "E013".to_string(),
            CompileError::CallableUsedAsValue => "E014".to_string(),
            CompileError::NonCallableLocal(_) => "E015".to_string(),
            CompileError::LocalSlotOverflow(_) => "E016".to_string(),
            CompileError::FrameLocalLimitExceeded { .. } => "E017".to_string(),
            CompileError::CallableArityMismatch { .. } => "E018".to_string(),
            CompileError::InlineFunctionRecursion(_) => "E019".to_string(),
            CompileError::UnresolvedModuleCall => "E020".to_string(),
        })
    }

    // ------------------------------------------------------------------
    // Completions
    // ------------------------------------------------------------------

    /// Returns completion items at the given source position.
    ///
    /// Completions respect lexical visibility: only local variables,
    /// parameters, and function declarations that are visible at the
    /// given position are included. Catalog functions and resources
    /// are always available.
    ///
    /// Host completion detail/signature formats consistently show
    /// `Borrow`/`BorrowMut`/`TakeOwned` for resource parameters and
    /// `resource<key>` for resource schemas. Legal overloads remain separate
    /// deterministic candidates; no arbitrary name-only selection is performed.
    pub fn completions_at(&self, position: SourcePosition) -> Vec<SemanticCompletion> {
        let mut completions = Vec::new();

        // The cursor prefix and the namespace it is being typed inside come
        // exclusively from the lexer token stream carried on the frontend IR —
        // never from scanning source text.
        let (prefix, namespace) = self.cursor_context(position);

        // Visible local slots and functions from the smallest containing
        // lexical scope, walking current -> parents.
        let Some(parsed) = self.semantic_index.as_ref().map(|index| &index.parsed) else {
            return self.catalog_completions(
                position.source_id,
                prefix.as_str(),
                namespace.as_deref(),
            );
        };
        let Some(cursor_scope) = self.smallest_scope_at(position, parsed) else {
            return self.catalog_completions(
                position.source_id,
                prefix.as_str(),
                namespace.as_deref(),
            );
        };

        let scope_chain = self.scope_chain(cursor_scope, parsed);
        let (visible_locals, visible_funcs) = self.visible_bindings(position, &scope_chain, parsed);

        // 1. Visible local variables, ordered by scope depth then declaration
        //    order, deduplicated by name with the innermost binding winning.
        if let Some(index) = &self.semantic_index {
            for (name, (slot, depth, decl_order)) in &visible_locals {
                if !prefix.is_empty() && !name.starts_with(prefix.as_str()) {
                    continue;
                }
                let detail = index.slot_schema(*slot).map(|s| format!("{s}"));
                completions.push(SemanticCompletion {
                    label: name.clone(),
                    detail,
                    docs: None,
                    kind: CompletionItemKind::Variable,
                });
                let _ = (depth, decl_order);
            }
        }

        // 2. Function declarations from the scope chain (functions are
        //    hoisted, so every declaration in the chain is visible).
        for (name, index) in &visible_funcs {
            if !prefix.is_empty() && !name.starts_with(prefix.as_str()) {
                continue;
            }
            let decl = self.ir.functions.iter().find(|decl| decl.index == *index);
            let detail = decl.map(|decl| format!("fn({})", decl.args.join(", ")));
            completions.push(SemanticCompletion {
                label: name.clone(),
                detail,
                docs: None,
                kind: CompletionItemKind::Function,
            });
        }

        completions.extend(self.catalog_completions(
            position.source_id,
            prefix.as_str(),
            namespace.as_deref(),
        ));

        completions
    }

    /// The visible local bindings at `position`, walking the containing
    /// scope chain. Returns `(name, (slot, scope_depth, decl_order))` in
    /// deterministic order and a `(name, function_index)` map for hoisted
    /// functions.
    ///
    /// Shadowing rules:
    /// * Within the cursor's own scope, only declarations whose identifier
    ///   token ends at or before the cursor are visible; a later
    ///   re-declaration of the same name (same slot) replaces the earlier
    ///   one.
    /// * In ancestor scopes, every declaration whose identifier token ends
    ///   at or before the cursor is visible; the innermost scope wins on
    ///   name collisions.
    /// * Functions are predeclared (hoisted), so every function declaration
    ///   in the chain is visible regardless of position.
    fn visible_bindings(
        &self,
        position: SourcePosition,
        scope_chain: &[ScopeId],
        parsed: &crate::compiler::ir::ParsedSemanticIndex,
    ) -> (Vec<(String, (LocalSlot, usize, u32))>, Vec<(String, u16)>) {
        let mut locals: Vec<(String, (LocalSlot, usize, u32))> = Vec::new();
        let mut funcs: Vec<(String, u16)> = Vec::new();
        let mut seen_local_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut seen_func_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut seen_slots: std::collections::HashSet<LocalSlot> = std::collections::HashSet::new();

        for (depth, &scope_id) in scope_chain.iter().enumerate() {
            // Same-scope declarations: only those whose identifier starts at
            // or before the cursor are visible, with later re-declarations of
            // a name replacing earlier ones.
            let mut same_scope_by_name: std::collections::BTreeMap<String, (LocalSlot, u32)> =
                std::collections::BTreeMap::new();
            for decl in &parsed.local_decls {
                if decl.scope_id != scope_id {
                    continue;
                }
                // A declaration after the cursor (in this scope) is not yet
                // visible; the cursor on its own identifier is visible.
                if decl.ident_span.lo > position.offset {
                    continue;
                }
                same_scope_by_name.insert(decl.name.clone(), (decl.slot, decl.decl_order));
            }
            for (name, (slot, decl_order)) in same_scope_by_name {
                if seen_slots.insert(slot) || !seen_local_names.contains(&name) {
                    if seen_local_names.insert(name.clone()) {
                        locals.push((name, (slot, depth, decl_order)));
                    }
                }
            }

            // Functions: hoisted, all visible.
            let scope_functions = parsed
                .scopes
                .get(scope_id as usize)
                .map(|scope| scope.functions.clone())
                .unwrap_or_default();
            for function_index in scope_functions {
                let Some(decl) = parsed
                    .func_decls
                    .iter()
                    .find(|decl| decl.function_index == function_index)
                else {
                    continue;
                };
                if seen_func_names.insert(decl.name.clone()) {
                    funcs.push((decl.name.clone(), function_index));
                }
            }
        }

        locals.sort_by(|a, b| {
            let (_, (_, depth_a, order_a)) = a;
            let (_, (_, depth_b, order_b)) = b;
            depth_a.cmp(depth_b).then(order_a.cmp(order_b))
        });
        funcs.sort_by(|a, b| a.0.cmp(&b.0));
        (locals, funcs)
    }

    /// The smallest containing lexical scope at `position`: the scope with
    /// the smallest range containing the position, deterministic on ties by
    /// the earlier start offset.
    fn smallest_scope_at(
        &self,
        position: SourcePosition,
        parsed: &crate::compiler::ir::ParsedSemanticIndex,
    ) -> Option<ScopeId> {
        let mut best: Option<(usize, Span)> = None;
        for (id, scope) in parsed.scopes.iter().enumerate() {
            if !self.position_in_span(position, scope.range) {
                continue;
            }
            let candidate = (id, scope.range);
            best = Some(match best {
                None => candidate,
                Some((cur_id, cur_range)) => {
                    let cur_len = cur_range.hi - cur_range.lo;
                    let new_len = scope.range.hi - scope.range.lo;
                    if new_len < cur_len || (new_len == cur_len && scope.range.lo < cur_range.lo) {
                        candidate
                    } else {
                        (cur_id, cur_range)
                    }
                }
            });
        }
        best.map(|(id, _)| id as ScopeId)
    }

    /// The scope chain from `scope_id` to the root, inclusive, ordered
    /// innermost-first.
    fn scope_chain(
        &self,
        scope_id: ScopeId,
        parsed: &crate::compiler::ir::ParsedSemanticIndex,
    ) -> Vec<ScopeId> {
        let mut chain = Vec::new();
        let mut current = Some(scope_id);
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            chain.push(id);
            current = parsed
                .scopes
                .get(id as usize)
                .and_then(|scope| scope.parent);
        }
        chain
    }

    /// The cursor prefix and, when the cursor is typing a namespace member
    /// (`ns::mem` or `ns::`), the namespace alias being completed — derived
    /// exclusively from the lexer token stream.
    ///
    /// Returns `(prefix, namespace)` where `prefix` is the full typed text
    /// (including any `ns::` qualifier) and `namespace` is `Some(ns)` when
    /// the prefix is (or ends in) a namespace-member position. A cursor in
    /// whitespace yields an empty prefix.
    fn cursor_context(&self, position: SourcePosition) -> (String, Option<String>) {
        let tokens = &self.ir.lexer_tokens;
        // The token at or immediately before the cursor.
        let mut idx = tokens.len();
        for (i, token) in tokens.iter().enumerate() {
            if token.span.source_id != position.source_id {
                continue;
            }
            if token.span.lo <= position.offset && position.offset <= token.span.hi {
                idx = i;
                break;
            }
        }
        if idx == tokens.len() {
            // No token touches the cursor (whitespace): empty prefix.
            return (String::new(), None);
        }

        let is_ident = |t: &crate::compiler::ir::LexerToken| t.kind == "Ident";
        let is_colon = |t: &crate::compiler::ir::LexerToken| t.kind == "Colon";

        // A cursor exactly on the trailing `::` of a namespace prefix
        // (`ns::` with nothing typed yet, cursor on the second Colon) is a
        // namespace-member position with an empty member prefix: the walk
        // below would start expecting an identifier at a Colon and break, so
        // detect the trailing pair first.
        if is_colon(&tokens[idx]) {
            let mut cursor = idx;
            // Consume the current and any adjacent Colon tokens forming the
            // trailing `::` (cursor may sit on either of the two).
            while cursor > 0 && is_colon(&tokens[cursor - 1]) {
                cursor -= 1;
            }
            if is_colon(&tokens[cursor]) {
                // Skip the whole trailing `::` pair (two Colons).
                let mut pair_end = cursor;
                while pair_end < tokens.len() && is_colon(&tokens[pair_end]) {
                    pair_end += 1;
                }
                if pair_end - cursor >= 2 && cursor >= 1 && is_ident(&tokens[cursor - 1]) {
                    let mut segments: Vec<String> = Vec::new();
                    let mut walk = cursor - 1;
                    let mut expect_ident = true;
                    loop {
                        let Some(token) = tokens.get(walk) else {
                            break;
                        };
                        if token.span.source_id != position.source_id {
                            break;
                        }
                        if expect_ident {
                            if is_ident(token) {
                                segments.push(token.ident.clone());
                                if walk == 0 {
                                    break;
                                }
                                walk -= 1;
                                expect_ident = false;
                            } else {
                                break;
                            }
                        } else if is_colon(token) {
                            if walk == 0 || !is_colon(&tokens[walk - 1]) {
                                break;
                            }
                            walk -= 2;
                            expect_ident = true;
                        } else {
                            break;
                        }
                    }
                    segments.reverse();
                    // `ns::` -> prefix `ns::`, namespace `ns`, empty member.
                    let joined = segments.join("::");
                    let namespace = if segments.len() >= 1 {
                        Some(segments.join("::"))
                    } else {
                        None
                    };
                    return (format!("{joined}::"), namespace);
                }
            }
        }

        // Walk left from the cursor collecting `ident (:: ident)*` segments.
        let mut segments: Vec<String> = Vec::new();
        let mut cursor = idx;
        let mut expect_ident = true;
        loop {
            let Some(token) = tokens.get(cursor) else {
                break;
            };
            if token.span.source_id != position.source_id {
                break;
            }
            if expect_ident {
                if is_ident(token) {
                    segments.push(token.ident.clone());
                    if cursor == 0 {
                        break;
                    }
                    cursor -= 1;
                    expect_ident = false;
                } else {
                    break;
                }
            } else if is_colon(token) {
                // `::` is two Colon tokens; require the pair.
                if cursor == 0 || !is_colon(&tokens[cursor - 1]) {
                    break;
                }
                cursor -= 2;
                expect_ident = true;
            } else {
                break;
            }
        }
        segments.reverse();
        let joined = segments.join("::");
        // The namespace being completed is everything before the final
        // segment: for `a::b::c` that is `a::b`; for `ns::member` it is `ns`.
        let namespace = if segments.len() >= 2 {
            Some(segments[..segments.len() - 1].join("::"))
        } else {
            None
        };
        (joined, namespace)
    }

    /// Catalog completions visible at the query source.
    ///
    /// When the IR carries parser provenance (`CatalogVisibility`), only the
    /// structured imports are offered: direct host call aliases (label = the
    /// local alias, detail = the canonical schema), wildcard host imports
    /// (all members of the imported namespace), host namespace aliases
    /// (namespace member completion), and file-module namespace aliases
    /// (module member completion against the merged flat functions, scoped
    /// to exactly the aliased module's exports). The whole catalog is never
    /// appended. IR without provenance (hand-built test models, plugin
    /// frontends that supply no structured metadata) yields the exact empty
    /// surface: no full-catalog fallback leaks into a frontend that imported
    /// nothing.
    fn catalog_completions(
        &self,
        source_id: SourceId,
        prefix: &str,
        namespace: Option<&str>,
    ) -> Vec<SemanticCompletion> {
        let prefix = prefix;
        let mut completions = Vec::new();
        let source_name = self
            .sources
            .file(source_id)
            .map(|file| file.name.clone())
            .unwrap_or_default();

        let Some(visibility) = &self.ir.catalog_visibility else {
            // No parser provenance: the surface is empty. A real plugin or
            // hand-built IR that provides no structured catalog metadata must
            // not receive a full-catalog fallback — that would leak the whole
            // host API catalog into a frontend that imported nothing. Lexical
            // and plugin completions also stay empty unless the plugin
            // supplies structured metadata on its IR.
            return completions;
        };

        // Namespace member completion: `ns::member` — resolve the canonical
        // namespace identity and list its members.
        if let Some(ns) = namespace {
            return self.namespace_member_completions(ns, prefix, visibility, &source_name);
        }

        // Direct host call aliases: `use io::{read as r};` -> `r`. A canonical
        // name may resolve to several catalog overloads; every matching
        // function surfaces as its own candidate with the alias label.
        for (alias, canonical) in &visibility.direct_host_call_aliases {
            if !prefix.is_empty() && !alias.starts_with(prefix) {
                continue;
            }
            for func in self
                .catalog
                .functions()
                .iter()
                .filter(|f| f.name == *canonical)
            {
                completions.push(SemanticCompletion {
                    label: alias.clone(),
                    // Canonical detail: the resolved schema prefixed with the
                    // canonical name so the alias's target is unambiguous.
                    detail: Some(format!(
                        "{canonical} — {}",
                        self.format_host_function_detail(func)
                    )),
                    docs: Some(func.description.clone()),
                    kind: CompletionItemKind::Function,
                });
            }
        }

        // Wildcard host imports: `use io::*;` -> every `io::*` member as a
        // direct name.
        for ns in &visibility.direct_host_wildcard_imports {
            for func in self.catalog.functions() {
                if let Some(member) = func.name.strip_prefix(&format!("{ns}::")) {
                    if !prefix.is_empty() && !member.starts_with(prefix) {
                        continue;
                    }
                    completions.push(SemanticCompletion {
                        label: member.to_string(),
                        detail: Some(self.format_host_function_detail(func)),
                        docs: Some(func.description.clone()),
                        kind: CompletionItemKind::Function,
                    });
                }
            }
        }

        // Host namespace aliases: `use prov as p;` -> the alias itself so the
        // user can continue typing `p::`.
        for (alias, canonical) in &visibility.host_namespace_aliases {
            if !prefix.is_empty() && !alias.starts_with(prefix) {
                continue;
            }
            completions.push(SemanticCompletion {
                label: alias.clone(),
                detail: Some(format!("namespace {canonical}")),
                docs: None,
                kind: CompletionItemKind::Keyword,
            });
        }

        // File-module namespace aliases, source-isolated by owning source.
        for alias in &visibility.module_namespace_aliases {
            if alias.source != source_name {
                continue;
            }
            if !prefix.is_empty() && !alias.alias.starts_with(prefix) {
                continue;
            }
            completions.push(SemanticCompletion {
                label: alias.alias.clone(),
                detail: Some(format!("module {}", alias.module_path)),
                docs: None,
                kind: CompletionItemKind::Keyword,
            });
        }

        completions
    }

    /// Member completions for `ns::member` where `ns` is a host namespace
    /// alias or a file-module namespace alias visible at the query source.
    fn namespace_member_completions(
        &self,
        ns: &str,
        prefix: &str,
        visibility: &CatalogVisibility,
        source_name: &str,
    ) -> Vec<SemanticCompletion> {
        let member_prefix = prefix
            .strip_prefix(&format!("{ns}::"))
            .unwrap_or(prefix)
            .to_string();
        let mut completions = Vec::new();

        // Host namespace alias: resolve the canonical namespace and list its
        // catalog members with their canonical schema detail.
        if let Some((_, canonical)) = visibility
            .host_namespace_aliases
            .iter()
            .find(|(alias, _)| alias == ns)
        {
            for func in self.catalog.functions() {
                if let Some(member) = func.name.strip_prefix(&format!("{canonical}::")) {
                    if !member_prefix.is_empty() && !member.starts_with(&member_prefix) {
                        continue;
                    }
                    completions.push(SemanticCompletion {
                        label: member.to_string(),
                        detail: Some(self.format_host_function_detail(func)),
                        docs: Some(func.description.clone()),
                        kind: CompletionItemKind::Function,
                    });
                }
            }
            return completions;
        }

        // File-module namespace alias: list the merged flat functions owned
        // by the alias's module. The alias's owning source isolates it from
        // same-named aliases in other units, and the resolved module source
        // (from `module_path` relative to the importing file's directory)
        // scopes the member list to exactly the aliased module — no other
        // imported module's exports leak into `ns::`.
        let Some(alias) = visibility
            .module_namespace_aliases
            .iter()
            .find(|alias| alias.alias == ns && alias.source == source_name)
        else {
            return completions;
        };
        let Some(module_source) = self.resolve_module_source(&alias.module_path, source_name)
        else {
            return completions;
        };
        for decl in &self.ir.functions {
            if !decl.exported || decl.symbol.is_none() {
                continue;
            }
            let owned_by_module = self
                .ir
                .function_sources
                .get(&decl.index)
                .map(|source| source == &module_source)
                .unwrap_or(false);
            if !owned_by_module {
                continue;
            }
            if !member_prefix.is_empty() && !decl.name.starts_with(&member_prefix) {
                continue;
            }
            let detail = Some(format!("fn({})", decl.args.join(", ")));
            completions.push(SemanticCompletion {
                label: decl.name.clone(),
                detail,
                docs: None,
                kind: CompletionItemKind::Function,
            });
        }
        completions
    }

    /// Resolve a module namespace alias's `module_path` (parser-relative
    /// spelling such as `a::util` or `self::c`) to the owning module's source
    /// name, mirroring the source loader's path resolution: the module path
    /// is joined to the importing source's directory, normalized, and
    /// canonicalized when the file exists on disk (the loader records the
    /// canonical identity for on-disk modules, and the lexical normalized
    /// path for virtual/source-override modules). `None` when the importing
    /// source is not a registered file path.
    fn resolve_module_source(&self, module_path: &str, importing_source: &str) -> Option<String> {
        let importing = std::path::Path::new(importing_source);
        let parent = importing.parent()?;
        // Translate leading `self`/`super` qualifiers and the `.rss`
        // extension exactly like the source loader's `use_path_to_spec`,
        // sharing the same routine so the semantic model and the loader
        // cannot drift on qualified import spellings (`self::nested`,
        // `super::shared`, `self::super::x`). The parser records the joined
        // spelling, so the string-based helper applies the identical
        // leading-qualifier rules as the structured loader path.
        let spec = super::modules::use_path_string_to_spec(module_path);
        let mut path = parent.join(spec);
        if path.extension().is_none() {
            path.set_extension("rss");
        }
        let normalized = normalize_module_path(path);
        let identity = if normalized.is_file() {
            normalized.canonicalize().unwrap_or(normalized)
        } else {
            normalized
        };
        Some(identity.display().to_string())
    }

    /// Format a host function's detail string for completions.
    /// Shows parameters with passing modes for resource types.
    fn format_host_function_detail(&self, func: &HostFunctionSchema) -> String {
        let param_strs: Vec<String> = func
            .params
            .iter()
            .map(|param| {
                let passing_label = match param.passing {
                    HostParamPassing::Value => String::new(),
                    HostParamPassing::Borrow => " borrow ".to_string(),
                    HostParamPassing::BorrowMut => " borrow_mut ".to_string(),
                    HostParamPassing::TakeOwned => " take ".to_string(),
                };
                format!("{}{}: {}", param.name, passing_label, param.ty,)
            })
            .collect();

        format!("fn({}) -> {}", param_strs.join(", "), func.return_type)
    }

    // ------------------------------------------------------------------
    // Go-to-definition
    // ------------------------------------------------------------------

    /// Returns the definition location for a symbol at the given position.
    ///
    /// For local variables, this returns the exact identifier span of their
    /// `let` binding (resolved through the parser's scope chain, so shadowed
    /// declarations resolve to the innermost visible one). For function
    /// declarations and function-value references, this returns the exact
    /// identifier span of the declared function (by resolved function target
    /// or module symbol — never by name search). For host function calls,
    /// this returns a virtual declaration entry from the catalog, keyed by
    /// the resolved schema identity carried on the call.
    ///
    /// Returns `None` when no definition can be determined.
    pub fn definition_at(&self, position: SourcePosition) -> Option<Definition> {
        // 1. Exact local declaration/reference identifier spans.
        if let Some(def) = self.definition_for_local_at(position) {
            return Some(def);
        }

        // 2. Function declaration/reference exact identifier spans and call
        //    targets (function index, local slot, module symbol, host schema).
        if let Some(def) = self.definition_for_func_at(position) {
            return Some(def);
        }

        None
    }

    /// Find the definition of a local variable at the position using the
    /// parser's local declaration/reference sites only.
    fn definition_for_local_at(&self, position: SourcePosition) -> Option<Definition> {
        let index = self.semantic_index.as_ref()?;
        let parsed = &index.parsed;

        // If the position is on a declaration identifier, return itself.
        for decl in &parsed.local_decls {
            if self.position_in_span(position, decl.ident_span) {
                return Some(Definition {
                    span: decl.ident_span,
                    label: format!("let {}", decl.name),
                });
            }
        }

        // If the position is on a reference identifier, resolve the visible
        // declaration through the parser scope chain (shadowing-aware).
        for reference in &parsed.local_refs {
            if self.position_in_span(position, reference.ident_span) {
                let decl = self
                    .local_decl_visible_from(reference.slot, reference.scope_id)
                    .or_else(|| {
                        // A captured/param slot may have no matching scope
                        // ancestor decl; fall back to any decl for the slot
                        // (params and captures record a decl site in their
                        // own scope, so this is a rare residual case).
                        parsed
                            .local_decls
                            .iter()
                            .find(|d| d.slot == reference.slot)
                            .cloned()
                    })?;
                return Some(Definition {
                    span: decl.ident_span,
                    label: format!("let {}", decl.name),
                });
            }
        }

        None
    }

    /// Find the definition of a function at the position using the parser's
    /// function declaration/reference sites and call targets.
    fn definition_for_func_at(&self, position: SourcePosition) -> Option<Definition> {
        let index = self.semantic_index.as_ref()?;
        let parsed = &index.parsed;

        // If the position is on a function declaration identifier, return it.
        for decl in &parsed.func_decls {
            if self.position_in_span(position, decl.ident_span) {
                return Some(Definition {
                    span: decl.ident_span,
                    label: format!("fn {}", decl.name),
                });
            }
        }

        // If the position is on a function-value reference identifier, resolve
        // the target (flat function index or module symbol) to its visible
        // declaration — never a name search.
        for reference in &parsed.func_refs {
            if self.position_in_span(position, reference.ident_span) {
                return self.function_definition_for_target(&reference.target, reference.scope_id);
            }
        }

        // If the position is within a call site, resolve the call target.
        let info = self.smallest_call_at(position)?;
        let site = &info.site;
        match &site.target {
            ParsedCallTarget::Function(function_index) => {
                // Resolve through the scope chain first; a host/builtin call
                // (no visible decl) falls back to the resolved schema identity.
                if let Some(decl) = self.function_decl_visible_from(*function_index, site.scope_id)
                {
                    return Some(Definition {
                        span: decl.ident_span,
                        label: format!("fn {}", decl.name),
                    });
                }
                self.host_definition_for_call(info)
            }
            ParsedCallTarget::Local(slot) => {
                let decl = self.local_decl_visible_from(*slot, site.scope_id)?;
                Some(Definition {
                    span: decl.ident_span,
                    label: format!("let {}", decl.name),
                })
            }
            ParsedCallTarget::Module(symbol) => self
                .function_definition_for_target(&FunctionRefTarget::Module(*symbol), site.scope_id),
            ParsedCallTarget::Unresolved => None,
        }
    }

    /// Resolve a [`FunctionRefTarget`] to its visible declaration span. Module
    /// targets resolve through the flat function table by symbol identity —
    /// never by name search.
    fn function_definition_for_target(
        &self,
        target: &FunctionRefTarget,
        from_scope: ScopeId,
    ) -> Option<Definition> {
        match target {
            FunctionRefTarget::Function(function_index) => {
                let decl = self.function_decl_visible_from(*function_index, from_scope)?;
                Some(Definition {
                    span: decl.ident_span,
                    label: format!("fn {}", decl.name),
                })
            }
            FunctionRefTarget::Module(symbol) => {
                // Find the flat function whose declaration owns this symbol.
                let function_index = self
                    .ir
                    .functions
                    .iter()
                    .find(|decl| decl.symbol == Some(*symbol))
                    .map(|decl| decl.index)?;
                // The merged flat index is unique to the module's declaration;
                // its scope lives in a different source tree, so resolve by
                // index without scope filtering (the symbol already names the
                // exact declaration).
                let decl = self
                    .semantic_index
                    .as_ref()?
                    .parsed
                    .func_decls
                    .iter()
                    .find(|decl| decl.function_index == function_index)?;
                Some(Definition {
                    span: decl.ident_span,
                    label: format!("fn {}", decl.name),
                })
            }
        }
    }

    /// A virtual definition for a catalog-resolved call, keyed by the resolved
    /// schema identity carried on the call (name + arity), not a name-only
    /// catalog scan.
    fn host_definition_for_call(
        &self,
        info: &crate::compiler::ir::ResolvedCallInfo,
    ) -> Option<Definition> {
        let resolved = info.host.as_ref()?;
        // The resolved call carries the exact catalog schema identity.
        let schema = crate::host_api::HostFunctionSchema {
            name: resolved.name.clone(),
            params: resolved
                .params
                .iter()
                .zip(resolved.passing.iter())
                .map(|(param, passing)| crate::host_api::HostParamSchema {
                    name: param.name.clone(),
                    ty: self.compiler_schema_to_host_schema(&param.schema),
                    passing: *passing,
                })
                .collect(),
            return_type: self.compiler_schema_to_host_schema(&resolved.return_type),
            description: self
                .catalog
                .functions()
                .iter()
                .find(|f| f.name == resolved.name)
                .map(|f| f.description.clone())
                .unwrap_or_default(),
        };
        let key = format!("host://{}/{}", schema.name, schema.params.len());
        let span = info.site.callee_span;
        Some(Definition {
            span,
            label: format!("{key} — {}", schema.description),
        })
    }

    // ------------------------------------------------------------------
    // UTF-8 / line-column conversion helper
    // ------------------------------------------------------------------

    /// Convert a byte offset to (line, column) in the source file.
    /// Both line and column are 1-indexed. For LSP, subtract 1 from each.
    pub fn offset_to_line_col(&self, position: SourcePosition) -> Option<(usize, usize)> {
        self.sources
            .line_col_for_offset(position.source_id, position.offset)
    }

    /// Convert a (line, column) pair to a byte offset.
    /// Both line and column are 1-indexed.
    pub fn line_col_to_offset(
        &self,
        source_id: SourceId,
        line: usize,
        col: usize,
    ) -> Option<usize> {
        self.sources.line_col_to_offset(source_id, line, col)
    }

    /// Convert a byte offset to a UTF-16 code-unit offset for LSP.
    /// This is needed because LSP uses UTF-16 code units for column offsets,
    /// while this crate uses UTF-8 byte offsets.
    pub fn offset_to_utf16_column(&self, position: SourcePosition) -> Option<usize> {
        let file = self.sources.file(position.source_id)?;
        let (line, _) = file.line_col_for_offset(position.offset)?;
        let line_start = file.line_span(line)?;
        let line_text = &file.text[line_start.start..position.offset.min(file.text.len())];
        // Count UTF-16 code units in the slice up to the offset.
        let mut utf16_col = 0usize;
        for ch in line_text.chars() {
            utf16_col += ch.len_utf16();
        }
        Some(utf16_col)
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Check if a position falls within a span.
    fn position_in_span(&self, position: SourcePosition, span: Span) -> bool {
        if position.source_id != span.source_id {
            return false;
        }
        // Half-open containment: an offset at `hi` (one past the identifier)
        // does not belong to the span, so adjacent tokens never both claim a
        // cursor position. Zero-length spans never match.
        position.offset >= span.lo && position.offset < span.hi
    }
}

/// Pick the smaller containing span between two candidates, deterministically.
/// `best` is `None` on the first candidate. Ties resolve by the shorter span
/// length, then the earlier start offset, then the later end offset.
fn pick_smaller_span<'a, T>(
    best: &'a Option<(T, LocalSlot, Span)>,
    candidate: &'a (T, LocalSlot, Span),
) -> &'a (T, LocalSlot, Span) {
    match best {
        None => candidate,
        Some(cur) => {
            let cur_len = cur.2.hi - cur.2.lo;
            let new_len = candidate.2.hi - candidate.2.lo;
            if new_len < cur_len || (new_len == cur_len && candidate.2.lo < cur.2.lo) {
                candidate
            } else {
                cur
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TypeSchema display for hover
// ---------------------------------------------------------------------------

impl std::fmt::Display for TypeSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeSchema::Unknown => write!(f, "unknown"),
            TypeSchema::Null => write!(f, "null"),
            TypeSchema::Int => write!(f, "int"),
            TypeSchema::Float => write!(f, "float"),
            TypeSchema::Number => write!(f, "number"),
            TypeSchema::Bool => write!(f, "bool"),
            TypeSchema::String => write!(f, "string"),
            TypeSchema::Bytes => write!(f, "bytes"),
            TypeSchema::Optional(inner) => write!(f, "optional<{inner}>"),
            TypeSchema::GenericParam(name) => write!(f, "{name}"),
            TypeSchema::Named(name, args) => {
                if args.is_empty() {
                    write!(f, "{name}")
                } else {
                    let args_str: Vec<String> = args.iter().map(|a| format!("{a}")).collect();
                    write!(f, "{name}<{}>", args_str.join(", "))
                }
            }
            TypeSchema::Array(inner) => write!(f, "array<{inner}>"),
            TypeSchema::ArrayTuple(items) => {
                let items_str: Vec<String> = items.iter().map(|i| format!("{i}")).collect();
                write!(f, "[{}]", items_str.join(", "))
            }
            TypeSchema::ArrayTupleRest { prefix, rest } => {
                let prefix_str: Vec<String> = prefix.iter().map(|p| format!("{p}")).collect();
                write!(f, "[{}, ..{rest}]", prefix_str.join(", "))
            }
            TypeSchema::Map(inner) => write!(f, "map<{inner}>"),
            TypeSchema::Object(fields) => {
                let fields_str: Vec<String> = fields
                    .iter()
                    .map(|(name, schema)| format!("{name}: {schema}"))
                    .collect();
                write!(f, "{{ {} }}", fields_str.join(", "))
            }
            TypeSchema::Callable { params, result } => {
                let params_str: Vec<String> = params.iter().map(|p| format!("{p}")).collect();
                write!(f, "fn({}) -> {result}", params_str.join(", "))
            }
            TypeSchema::Resource(key) => write!(f, "resource<{key}>"),
        }
    }
}

impl std::fmt::Display for SourcePosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source {} @ offset {}", self.source_id, self.offset)
    }
}

impl std::fmt::Display for SemanticDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref span) = self.span {
            write!(
                f,
                "{} (source {} [{}..{}])",
                self.message, span.source_id, span.lo, span.hi
            )
        } else {
            write!(f, "{}", self.message)
        }
    }
}

/// Normalize a module path by removing `.` components and collapsing `..`
/// lexically, mirroring the source loader's normalization so the semantic
/// model's module-source resolution matches the recorded `function_sources`.
fn normalize_module_path(path: std::path::PathBuf) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match normalized.components().next_back() {
                Some(std::path::Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(std::path::Component::ParentDir) | None => {
                    normalized.push(component.as_os_str())
                }
                Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {}
                Some(std::path::Component::CurDir) => {}
            },
            std::path::Component::RootDir
            | std::path::Component::Prefix(_)
            | std::path::Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::{
        Expr, LocalDeclSite, ParsedCallSite, ParsedCallTarget, ParsedLexicalScope,
        ParsedSemanticIndex, SemanticNodeId, Stmt,
    };

    use crate::host_api::{
        HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
        HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
    };

    /// Build a minimal standard catalog for testing.
    fn test_catalog() -> Arc<HostApiCatalog> {
        let sqlite_key = ResourceTypeKey::new("sqlite.connection").unwrap();
        let io_file_key = ResourceTypeKey::new("io.file").unwrap();

        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(
            sqlite_key.clone(),
            "SQLite database connection",
        ));
        builder.resource(ResourceTypeSchema::new(
            io_file_key.clone(),
            "A file on disk",
        ));

        // sqlite::open(path: string) -> resource<sqlite.connection>
        builder.function(HostFunctionSchema::with_return(
            "sqlite::open",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
            HostTypeSchema::Resource(sqlite_key.clone()),
        ));

        // sqlite::query(connection: borrow resource<sqlite.connection>, sql: string) -> int
        builder.function(HostFunctionSchema::with_return(
            "sqlite::query",
            vec![
                HostParamSchema::with_passing(
                    "connection",
                    HostTypeSchema::Resource(sqlite_key),
                    HostParamPassing::Borrow,
                ),
                HostParamSchema::value("sql", HostTypeSchema::String),
            ],
            HostTypeSchema::Int,
        ));

        // io::open(path: string) -> resource<io.file>
        builder.function(HostFunctionSchema::with_return(
            "io::open",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
            HostTypeSchema::Resource(io_file_key.clone()),
        ));

        // len(string) -> int
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::String)],
            HostTypeSchema::Int,
        ));

        // len(array) -> int
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value(
                "value",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Unknown)),
            )],
            HostTypeSchema::Int,
        ));

        // len(bytes) -> int
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::Bytes)],
            HostTypeSchema::Int,
        ));

        // len(map) -> int
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value(
                "value",
                HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
            )],
            HostTypeSchema::Int,
        ));

        Arc::new(builder.build().expect("test catalog build"))
    }

    /// Build a minimal FrontendIr for testing position queries.
    fn test_ir() -> FrontendIr {
        FrontendIr {
            stmts: Vec::new(),
            locals: 0,
            local_bindings: Vec::new(),
            struct_schemas: std::collections::HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions: Vec::new(),
            function_impls: std::collections::HashMap::new(),
            stmt_sources: Vec::new(),
            function_sources: std::collections::HashMap::new(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        }
    }

    /// `test_ir` with structured catalog provenance: wildcard host imports for
    /// `sqlite`/`io` and a direct host call alias for `len`. This drives the
    /// exact structured completion path (no full-catalog fallback).
    fn test_ir_with_visibility() -> FrontendIr {
        let mut ir = test_ir();
        ir.catalog_visibility = Some(crate::compiler::ir::CatalogVisibility {
            host_namespace_aliases: vec![("sqlite".to_string(), "sqlite".to_string())],
            direct_host_call_aliases: vec![("len".to_string(), "len".to_string())],
            direct_host_wildcard_imports: vec!["sqlite".to_string(), "io".to_string()],
            module_namespace_aliases: Vec::new(),
            use_declarations: Vec::new(),
        });
        ir
    }

    // ------------------------------------------------------------------
    // Catalog fingerprint
    // ------------------------------------------------------------------

    #[test]
    fn catalog_fingerprint_is_exposed() {
        let catalog = test_catalog();
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog.clone(), Vec::new());
        let fp = model.catalog_fingerprint();
        assert_eq!(
            fp,
            catalog.fingerprint(),
            "fingerprint must match the catalog"
        );
    }

    // ------------------------------------------------------------------
    // Hover / inferred schema
    // ------------------------------------------------------------------

    #[test]
    fn inferred_schema_with_no_content_returns_none() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "let x = 42");
        let model = SemanticModel::new(test_ir(), sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 0);
        assert!(
            model.inferred_schema_at(pos).is_none(),
            "empty IR should return None"
        );
    }

    // ------------------------------------------------------------------
    // Completions include catalog functions
    // ------------------------------------------------------------------

    #[test]
    fn completions_include_catalog_functions() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "");
        let model = SemanticModel::new(test_ir_with_visibility(), sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 0);
        let completions = model.completions_at(pos);

        // Wildcard imports surface the imported namespaces' members as
        // direct names (`open`, `query` from sqlite/io), and the direct
        // alias surfaces `len` (4 overloads, all with the alias label).
        let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(
            names.contains(&"open"),
            "completions should include the wildcard member open: {:?}",
            names
        );
        assert!(
            names.contains(&"query"),
            "completions should include the wildcard member query: {:?}",
            names
        );
        assert!(
            names.contains(&"len"),
            "completions should include the direct alias len: {:?}",
            names
        );
        // The canonical `sqlite::open` full name is NOT offered when the
        // member is surfaced through the wildcard import as `open`.
        assert!(
            names.iter().all(|n| n != &"sqlite::open"),
            "canonical name must not appear alongside the wildcard member: {:?}",
            names
        );
        assert!(
            names.iter().all(|n| n != &"io::open"),
            "io::open canonical name must not leak: {:?}",
            names
        );
    }

    #[test]
    fn completions_include_catalog_resources() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "");
        let model = SemanticModel::new(test_ir_with_visibility(), sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 0);
        let completions = model.completions_at(pos);

        // The structured surface is import-driven: resources are only
        // reachable through a namespace alias member surface, never dumped
        // wholesale. With no `ns::` member query, no resource labels appear.
        let resource_completions: Vec<&SemanticCompletion> = completions
            .iter()
            .filter(|c| c.kind == CompletionItemKind::Resource)
            .collect();
        assert!(
            resource_completions.is_empty(),
            "no full-catalog resource leakage: {:?}",
            resource_completions
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn completions_detail_shows_resource_passing() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "");
        let model = SemanticModel::new(test_ir_with_visibility(), sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 0);
        let completions = model.completions_at(pos);

        // The wildcard import surfaces sqlite::query as `query`; its detail
        // must still show the borrow resource parameter.
        let query = completions
            .iter()
            .find(|c| c.label == "query")
            .expect("query member should be in completions");
        let detail = query.detail.as_deref().unwrap_or("");
        // The detail should show the borrow resource parameter
        assert!(
            detail.contains("borrow"),
            "sqlite::query detail should show borrow mode: {detail}"
        );
        assert!(
            detail.contains("resource<sqlite.connection>"),
            "sqlite::query detail should show resource type: {detail}"
        );
    }

    // ------------------------------------------------------------------
    // Diagnostics
    // ------------------------------------------------------------------

    #[test]
    fn diagnostics_with_no_errors_returns_empty() {
        let catalog = test_catalog();
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, Vec::new());
        let diags = model.diagnostics();
        assert!(
            diags.is_empty(),
            "no errors should produce empty diagnostics"
        );
    }

    #[test]
    fn diagnostics_includes_compile_errors() {
        let catalog = test_catalog();
        let errors = vec![CompileError::HostCallResolve {
            line: Some(1),
            source_name: Some("test".to_string()),
            detail: "expected resource<sqlite.connection>, found resource<io.file>".to_string(),
            span: None,
        }];
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, errors);
        let diags = model.diagnostics();
        assert_eq!(diags.len(), 1, "should have one diagnostic");
        assert!(
            diags[0].message.contains("sqlite.connection"),
            "diagnostic should mention sqlite.connection: {}",
            diags[0].message
        );
        assert!(
            diags[0].message.contains("io.file"),
            "diagnostic should mention io.file: {}",
            diags[0].message
        );
    }

    #[test]
    fn diagnostics_includes_error_code() {
        let catalog = test_catalog();
        let errors = vec![CompileError::HostCallResolve {
            line: Some(1),
            source_name: Some("test".to_string()),
            detail: "unknown host function".to_string(),
            span: None,
        }];
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, errors);
        let diags = model.diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code,
            Some("E001".to_string()),
            "HostCallResolve should have code E001"
        );
    }

    #[test]
    fn diagnostics_includes_span_when_source_name_matches() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test.rss", "let x = sqlite::open(\"db\");\n");
        let callee_span = Span::new(sid, 8, 20);
        let errors = vec![CompileError::HostCallResolve {
            line: Some(1),
            source_name: Some("test.rss".to_string()),
            detail: "expected resource<sqlite.connection>, found resource<io.file>".to_string(),
            span: Some(callee_span),
        }];
        let model = SemanticModel::new(test_ir(), sources, catalog, errors);
        let diags = model.diagnostics();
        assert_eq!(diags.len(), 1);
        let span = diags[0].span.expect("carried span returned verbatim");
        assert_eq!(span.source_id, sid);
        assert_eq!((span.lo, span.hi), (8, 20));
    }

    #[test]
    fn diagnostics_spanless_error_has_no_guessed_span() {
        // A synthetic error that carries no span must surface `None` — the
        // compiler never guesses a same-line token span from the source.
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test.rss", "let a = 1; let b = a;\n");
        let errors = vec![CompileError::HostCallResolve {
            line: Some(1),
            source_name: Some("test.rss".to_string()),
            detail: "spanless synthetic error".to_string(),
            span: None,
        }];
        let model = SemanticModel::new(test_ir(), sources, catalog, errors);
        let diags = model.diagnostics();
        assert_eq!(diags.len(), 1);
        let _ = sid;
        assert!(
            diags[0].span.is_none(),
            "a spanless error must not receive a guessed span: {:?}",
            diags[0].span
        );
    }

    // ------------------------------------------------------------------
    // TypeSchema display
    // ------------------------------------------------------------------

    #[test]
    fn type_schema_resource_display() {
        let key = ResourceTypeKey::new("sqlite.connection").unwrap();
        let schema = TypeSchema::Resource(key);
        assert_eq!(format!("{schema}"), "resource<sqlite.connection>");
    }

    #[test]
    fn type_schema_scalar_display() {
        assert_eq!(format!("{}", TypeSchema::Int), "int");
        assert_eq!(format!("{}", TypeSchema::String), "string");
        assert_eq!(format!("{}", TypeSchema::Bool), "bool");
        assert_eq!(format!("{}", TypeSchema::Null), "null");
        assert_eq!(format!("{}", TypeSchema::Unknown), "unknown");
    }

    #[test]
    fn type_schema_complex_display() {
        let key = ResourceTypeKey::new("io.file").unwrap();
        let schema = TypeSchema::Array(Box::new(TypeSchema::Resource(key)));
        assert_eq!(format!("{schema}"), "array<resource<io.file>>");
    }

    // ------------------------------------------------------------------
    // Signature help
    // ------------------------------------------------------------------

    #[test]
    fn callable_signature_with_no_calls_returns_none() {
        let catalog = test_catalog();
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, Vec::new());
        let pos = SourcePosition::new(0, 0);
        assert!(model.callable_signature_at(pos).is_none());
    }

    // ------------------------------------------------------------------
    // Definition
    // ------------------------------------------------------------------

    #[test]
    fn definition_at_returns_none_for_unknown_position() {
        let catalog = test_catalog();
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, Vec::new());
        let pos = SourcePosition::new(0, 0);
        assert!(model.definition_at(pos).is_none());
    }

    #[test]
    fn definition_at_returns_local_declaration() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "let x = 42");
        let mut ir = test_ir();
        ir.stmts.push(Stmt::Let {
            index: 0,
            declared_schema: None,
            expr: Expr::Int(42),
            line: 1,
        });
        ir.local_bindings.push(("x".to_string(), 0));
        ir.locals = 1;
        // Build parser provenance: one root scope and a declaration site for
        // 'x' at the exact identifier span 4..5.
        let mut parsed = ParsedSemanticIndex::default();
        parsed.scopes.push(ParsedLexicalScope {
            id: 0,
            parent: None,
            range: Span::new(sid, 0, 11),
            declarations: vec![0],
            functions: Vec::new(),
        });
        parsed.local_decls.push(LocalDeclSite {
            id: SemanticNodeId(0),
            ident_span: Span::new(sid, 4, 5),
            stmt_span: Span::new(sid, 0, 11),
            slot: 0,
            name: "x".to_string(),
            scope_id: 0,
            decl_order: 0,
        });
        ir.parsed_semantic_index = Some(parsed);
        ir.semantic_index = Some(SemanticIndex::build(vec![Some(TypeSchema::Int)], &ir));
        let model = SemanticModel::new(ir, sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 4); // cursor on 'x'
        let def = model.definition_at(pos);
        assert!(def.is_some(), "should find definition for 'x'");
        let def = def.expect("definition for 'x'");
        assert!(
            def.label.contains("x"),
            "label should mention 'x': {}",
            def.label
        );
        assert_eq!(def.span.lo, 4, "definition span starts at offset 4");
        assert_eq!(def.span.hi, 5, "definition span ends at offset 5");
        assert_eq!(def.span.source_id, sid, "definition span names the source");
    }

    // ------------------------------------------------------------------
    // UTF-8 / line-column conversion
    // ------------------------------------------------------------------

    #[test]
    fn offset_to_line_col_returns_correct_values() {
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "let x = 42\nlet y = 43\n");
        let model = SemanticModel::new(test_ir(), sources, test_catalog(), Vec::new());
        // First line, first character.
        let (line, col) = model
            .offset_to_line_col(SourcePosition::new(sid, 0))
            .unwrap();
        assert_eq!(line, 1, "first char should be line 1");
        assert_eq!(col, 1, "first char should be column 1");
        // Second line, first character (offset 11 is start of "let y = 43\n").
        let (line, col) = model
            .offset_to_line_col(SourcePosition::new(sid, 11))
            .unwrap();
        assert_eq!(line, 2, "second line should be line 2");
        assert_eq!(col, 1, "first char of second line should be column 1");
    }

    #[test]
    fn line_col_to_offset_roundtrips() {
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "let x = 42\nlet y = 43\n");
        let model = SemanticModel::new(test_ir(), sources, test_catalog(), Vec::new());
        let offset = model.line_col_to_offset(sid, 1, 1).unwrap();
        assert_eq!(offset, 0);
        let offset = model.line_col_to_offset(sid, 2, 1).unwrap();
        assert_eq!(offset, 11);
    }

    // ------------------------------------------------------------------
    // Overloads: len has 4 overloads
    // ------------------------------------------------------------------

    #[test]
    fn completions_include_len_overloads() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "");
        let model = SemanticModel::new(test_ir_with_visibility(), sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 0);
        let completions = model.completions_at(pos);

        // len is a direct host call alias; the catalog has 4 len overloads,
        // each surfaced as a separate candidate with the alias label.
        let len_completions: Vec<&SemanticCompletion> =
            completions.iter().filter(|c| c.label == "len").collect();
        assert_eq!(
            len_completions.len(),
            4,
            "len should have 4 overload completions (string, array, bytes, map)"
        );
    }

    // ------------------------------------------------------------------
    // Custom external catalog
    // ------------------------------------------------------------------

    #[test]
    fn custom_catalog_works_identically() {
        let custom_key = ResourceTypeKey::new("custom.resource").unwrap();
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(
            custom_key.clone(),
            "Custom resource",
        ));
        builder.function(HostFunctionSchema::with_return(
            "custom::create",
            vec![HostParamSchema::value("name", HostTypeSchema::String)],
            HostTypeSchema::Resource(custom_key),
        ));
        let catalog = Arc::new(builder.build().expect("custom catalog build"));

        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "");
        let mut ir = test_ir();
        ir.catalog_visibility = Some(crate::compiler::ir::CatalogVisibility {
            host_namespace_aliases: vec![("custom".to_string(), "custom".to_string())],
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: Vec::new(),
        });
        let model = SemanticModel::new(ir, sources.clone(), catalog.clone(), Vec::new());
        let pos = SourcePosition::new(sid, 0);
        let completions = model.completions_at(pos);

        let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(
            names.contains(&"custom"),
            "custom namespace alias should appear in completions"
        );
        let namespace = SourcePosition::new(sid, 6);
        let _ = namespace;
        // Member completion through the alias: cursor inside `custom::cr`.
        let member_ir = {
            let mut ir = test_ir();
            ir.catalog_visibility = Some(crate::compiler::ir::CatalogVisibility {
                host_namespace_aliases: vec![("custom".to_string(), "custom".to_string())],
                direct_host_call_aliases: Vec::new(),
                direct_host_wildcard_imports: Vec::new(),
                module_namespace_aliases: Vec::new(),
                use_declarations: Vec::new(),
            });
            ir.lexer_tokens = vec![
                crate::compiler::ir::LexerToken {
                    kind: "Ident".to_string(),
                    ident: "custom".to_string(),
                    span: Span::new(sid, 0, 6),
                },
                crate::compiler::ir::LexerToken {
                    kind: "Colon".to_string(),
                    ident: String::new(),
                    span: Span::new(sid, 6, 7),
                },
                crate::compiler::ir::LexerToken {
                    kind: "Colon".to_string(),
                    ident: String::new(),
                    span: Span::new(sid, 7, 8),
                },
                crate::compiler::ir::LexerToken {
                    kind: "Ident".to_string(),
                    ident: "cr".to_string(),
                    span: Span::new(sid, 8, 10),
                },
            ];
            ir
        };
        let model = SemanticModel::new(member_ir, sources, catalog, Vec::new());
        let completions = model.completions_at(SourcePosition::new(sid, 9));
        let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(
            names.contains(&"create"),
            "custom::cr should resolve the create member: {:?}",
            names
        );
    }

    // ------------------------------------------------------------------
    // Wrong resource type diagnostic
    // ------------------------------------------------------------------

    #[test]
    fn wrong_resource_type_diagnostic() {
        let catalog = test_catalog();
        let errors = vec![CompileError::HostCallResolve {
            line: Some(5),
            source_name: Some("test.rss".to_string()),
            detail: "no host function `sqlite::query` matches the arguments: \
                         expected resource<sqlite.connection> for parameter `connection`, \
                         found resource<io.file>"
                .to_string(),
            span: None,
        }];
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, errors);
        let diags = model.diagnostics();
        assert_eq!(diags.len(), 1);
        let msg = &diags[0].message;
        assert!(
            msg.contains("sqlite.connection"),
            "wrong resource diagnostic should mention expected key: {msg}"
        );
        assert!(
            msg.contains("io.file"),
            "wrong resource diagnostic should mention actual key: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // Unknown host API diagnostic
    // ------------------------------------------------------------------

    #[test]
    fn unknown_host_api_diagnostic() {
        let catalog = test_catalog();
        let errors = vec![CompileError::HostCallResolve {
            line: Some(3),
            source_name: Some("test.rss".to_string()),
            detail: "unknown host function `nonexistent::func`".to_string(),
            span: None,
        }];
        let model = SemanticModel::new(test_ir(), SourceMap::new(), catalog, errors);
        let diags = model.diagnostics();
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].message.contains("nonexistent::func"),
            "unknown host diagnostic should mention the function name: {}",
            diags[0].message
        );
        assert_eq!(
            diags[0].code,
            Some("E001".to_string()),
            "unknown host should have code E001"
        );
    }

    // ------------------------------------------------------------------
    // Completions respect prefix filtering
    // ------------------------------------------------------------------

    #[test]
    fn completions_filter_by_prefix() {
        let catalog = test_catalog();
        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "qu");
        let mut ir = test_ir_with_visibility();
        // Carry the lexer token stream so the prefix comes from token spans.
        ir.lexer_tokens = vec![crate::compiler::ir::LexerToken {
            kind: "Ident".to_string(),
            ident: "qu".to_string(),
            span: Span::new(sid, 0, 2),
        }];
        let model = SemanticModel::new(ir, sources, catalog, Vec::new());
        // Position at offset 2 (after "qu")
        let pos = SourcePosition::new(sid, 2);
        let completions = model.completions_at(pos);
        let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        // The sqlite wildcard import offers member `query` (matches "qu").
        assert!(
            names.contains(&"query"),
            "completions should include sqlite::query's member with prefix 'qu': {:?}",
            names
        );
        // Should NOT include open (doesn't start with "qu") nor len (doesn't
        // match the prefix).
        assert!(
            !names.contains(&"open"),
            "completions should NOT include open with prefix 'qu': {:?}",
            names
        );
        assert!(
            !names.contains(&"len"),
            "completions should NOT include len with prefix 'qu': {:?}",
            names
        );
    }

    // ------------------------------------------------------------------
    // Signature help with description
    // ------------------------------------------------------------------

    #[test]
    fn callable_signature_includes_description() {
        // Create a catalog with description
        let key = ResourceTypeKey::new("test.resource").unwrap();
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(key.clone(), "A test resource"));
        let mut func = HostFunctionSchema::with_return(
            "test::open",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
            HostTypeSchema::Resource(key),
        );
        func.description = "Opens a test resource".to_string();
        builder.function(func);
        let catalog = Arc::new(builder.build().expect("test catalog"));

        // Build an IR with a call to test::open carrying parser provenance
        // (SemanticNodeId(0)) so the semantic index can pair the typed node
        // with its parsed call site.
        let mut ir = test_ir();
        let resolved = ResolvedHostCall {
            name: "test::open".to_string(),
            params: vec![crate::compiler::ir::ResolvedHostParam {
                name: "path".to_string(),
                schema: TypeSchema::String,
            }],
            return_type: TypeSchema::Resource(ResourceTypeKey::new("test.resource").unwrap()),
            passing: vec![HostParamPassing::Value],
            fingerprint: catalog.fingerprint(),
        };
        ir.stmts.push(Stmt::Expr {
            expr: Expr::Call(
                0,
                Vec::new(),
                Vec::new(),
                Some(Box::new(resolved)),
                Some(SemanticNodeId(0)),
            ),
            line: 1,
        });
        ir.locals = 0;

        let mut sources = SourceMap::new();
        let sid = sources.add_source("test", "test::open(\"test\");\n");
        // Parser provenance for the call site: callee span 0..9, expr 0..17.
        let mut parsed = ParsedSemanticIndex::default();
        parsed.scopes.push(ParsedLexicalScope {
            id: 0,
            parent: None,
            range: Span::new(sid, 0, 17),
            declarations: Vec::new(),
            functions: Vec::new(),
        });
        parsed.call_sites.push(ParsedCallSite {
            id: SemanticNodeId(0),
            callee_span: Span::new(sid, 0, 9),
            expr_span: Span::new(sid, 0, 17),
            target: ParsedCallTarget::Function(0),
            name: "test::open".to_string(),
            scope_id: 0,
            is_namespace_call: true,
        });
        ir.parsed_semantic_index = Some(parsed);
        ir.semantic_index = Some(SemanticIndex::build(Vec::new(), &ir));

        let model = SemanticModel::new(ir, sources, catalog, Vec::new());
        let pos = SourcePosition::new(sid, 4);
        let signature = model.callable_signature_at(pos);
        assert!(
            signature.is_some(),
            "should find a signature for test::open"
        );
        let sig = signature.expect("signature for test::open");
        assert!(
            !sig.description.is_empty(),
            "description should not be empty: got '{}'",
            sig.description
        );
    }
}
