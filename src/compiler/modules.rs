//! Compiler-owned module identities and the semantic module graph.
//!
//! Milestones 1-6 of the semantic module system: every source that takes
//! part in a compilation is assigned a deterministic [`ModuleId`] and
//! [`SourceId`], `use` directives are parsed into structured [`UseDecl`]
//! nodes with spans and clauses, the source loader records resolved import
//! edges in a [`ModuleGraph`], and every declaration receives a
//! [`SymbolId`] owned by its module alongside an explicit public export
//! table and a separate imported-binding table. Identities never depend on
//! a file stem alone: two modules with the same basename in different
//! directories are distinct nodes, and re-visiting the same module identity
//! reuses the same node.
//!
//! Since milestone 6 the semantic graph is the *sole* file-module path: the
//! textual import rewriting, the synthetic imported-function prelude, and
//! the prelude line-map remapping are removed. Call sites resolve to
//! [`SymbolId`]s in the source loader and the linker merges units by symbol
//! identity, applying deterministic flat-boundary mangling only at the final
//! bytecode boundary.
//!
//! [`SourceId`] here is the module graph's own identity space, distinct from
//! `source_map::SourceId` (which is assigned per-unit by ad hoc `SourceMap`
//! instances). Milestone 5 reconciles the two spaces: every module's raw
//! text is registered in the compilation-wide `SourceMap` at its graph
//! `SourceId`, so spans survive unit merge with their owning source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::SourcePathError;
use super::frontends::{is_ident_continue, is_ident_start};
use super::source_loader::ImportClause;
use super::source_map::Span;

/// Deterministic identity of one module within a single compilation.
///
/// Assigned in discovery order: the root unit is always `ModuleId(0)`, and
/// every discovered module gets the next unused id the first time its
/// canonical identity is registered. Re-importing the same module yields the
/// same id; two modules that merely share a file stem are distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub u32);

/// Deterministic identity of one parsed source text within a compilation.
///
/// Distinct from `source_map::SourceId`: the module graph hands out its own
/// monotonic ids so that graph edges can reference sources without depending
/// on per-unit `SourceMap` construction order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

/// Deterministic identity of one declaration within a compilation.
///
/// Composed of the owning [`ModuleId`] and a module-local index, so two
/// same-named declarations in independent modules never collide. Milestone 3
/// assigns symbol ids to declarations; the type is defined here so the whole
/// identity surface lands in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId {
    pub module: ModuleId,
    pub index: u32,
}

/// One segment of a structured `use` path.
///
/// `self`/`super` are only classified as qualifiers while they lead the path,
/// mirroring the legacy line-based resolver: a `self` appearing mid-path is a
/// literal file segment (e.g. `use a::self::b;` resolves `a/self/b.rss`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsePathSegment {
    Self_,
    Super,
    Ident(String),
}

/// A structured `use` directive parsed from RustScript source.
///
/// Carries the full path (including `self`/`super` qualifiers), the import
/// clause, the exact source span of the directive, and the directive line.
/// The source loader consumes these nodes for discovery instead of treating
/// line-prefix stripping as the authoritative import parser.
#[derive(Clone, Debug)]
pub struct UseDecl {
    pub path: Vec<UsePathSegment>,
    pub clause: ImportClause,
    pub span: Span,
    pub line: usize,
}

/// Classification of one resolved import edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportTargetKind {
    /// A RustScript file module loaded from disk or an override.
    FileModule,
    /// A virtual host namespace resolved on the dedicated host path.
    HostNamespace,
    /// A builtin namespace such as `io`, `json`, or `re`.
    BuiltinNamespace,
}

/// A resolved import edge inside a [`ModuleGraph`].
///
/// `target` is `Some(ModuleId)` once the destination module node is known
/// (`FileModule` edges), and `None` for host/builtin namespaces that stay on
/// their dedicated resolution paths.
#[derive(Clone, Debug)]
pub struct ResolvedImport {
    pub kind: ImportTargetKind,
    /// Normalized module specifier (e.g. `./nested.rss`).
    pub spec: String,
    pub clause: ImportClause,
    pub span: Span,
    pub line: usize,
    pub target: Option<ModuleId>,
}

/// One declaration owned by a module, with its deterministic [`SymbolId`].
///
/// Milestone 3: every declaration in a module receives a symbol whose
/// `module` is the owning [`ModuleId`] and whose `index` is the declaration's
/// position in the module's declaration table. Two same-named declarations in
/// independent modules therefore never share a symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclSymbol {
    pub symbol: SymbolId,
    pub name: String,
    /// `true` when the declaration is marked `pub` and appears in the module's
    /// public export table.
    pub public: bool,
}

/// One entry of a module's public export table.
///
/// The table is populated exclusively from local public declarations:
/// imported bindings never appear here, so re-exporting another module's
/// functions requires an explicit mechanism and there is no implicit
/// transitive re-export through the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportEntry {
    pub name: String,
    pub symbol: SymbolId,
}

/// A binding introduced into a module by a resolved import edge.
///
/// Imported bindings are tracked separately from local declarations: they are
/// never part of [`ModuleNode::declarations`] and never enter
/// [`ModuleNode::exports`]. `local_name` is the name the importing module
/// binds (`as` alias for named imports, or the source name otherwise);
/// `source_name` is the declaration's name in the source module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedBinding {
    pub local_name: String,
    pub source_module: ModuleId,
    pub source_symbol: SymbolId,
    pub source_name: String,
}

/// One node of the [`ModuleGraph`]: a module, its resolved imports, and its
/// milestone-3 declaration/export/imported-binding tables.
///
/// Milestone 1-2 fills `imports` during source-loader discovery; milestone 3
/// fills `declarations`, `exports`, and `imported_bindings` once each module's
/// unit is parsed.
#[derive(Clone, Debug)]
pub struct ModuleNode {
    pub module: ModuleId,
    pub source: SourceId,
    /// Canonical disk identity (or normalized virtual identity) of the module.
    pub identity: PathBuf,
    /// Display name used in diagnostics.
    pub source_name: String,
    pub imports: Vec<ResolvedImport>,
    /// Local declarations in source order, each with its owned symbol.
    pub declarations: Vec<DeclSymbol>,
    /// Public export table: local `pub` declarations only.
    pub exports: Vec<ExportEntry>,
    /// Bindings introduced by import edges, separate from local declarations.
    pub imported_bindings: Vec<ImportedBinding>,
}

/// The module graph for one compilation.
///
/// Nodes are registered in deterministic discovery order; the first node is
/// always the root unit. `by_identity` guarantees that the same canonical
/// module identity maps to exactly one node, so modules with identical file
/// stems in different directories stay distinct while lexically equivalent
/// paths collapse.
#[derive(Default)]
pub struct ModuleGraph {
    nodes: Vec<ModuleNode>,
    by_identity: HashMap<PathBuf, ModuleId>,
    next_source: u32,
}

impl ModuleGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a module node for `identity`, or return the existing node id.
    ///
    /// The first call for the root unit yields `ModuleId(0)` / `SourceId(0)`;
    /// subsequent first-time registrations receive the next unused ids in
    /// call order.
    pub fn add_node(
        &mut self,
        identity: PathBuf,
        source_name: String,
        imports: Vec<ResolvedImport>,
    ) -> ModuleId {
        if let Some(existing) = self.by_identity.get(&identity) {
            return *existing;
        }
        let module = ModuleId(u32::try_from(self.nodes.len()).unwrap_or(u32::MAX));
        let source = SourceId(self.next_source);
        self.next_source = self.next_source.saturating_add(1);
        self.by_identity.insert(identity.clone(), module);
        self.nodes.push(ModuleNode {
            module,
            source,
            identity,
            source_name,
            imports,
            declarations: Vec::new(),
            exports: Vec::new(),
            imported_bindings: Vec::new(),
        });
        module
    }

    pub fn node(&self, module: ModuleId) -> Option<&ModuleNode> {
        self.nodes.get(module.0 as usize)
    }

    pub fn nodes(&self) -> &[ModuleNode] {
        &self.nodes
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn module_id_for_identity(&self, identity: &Path) -> Option<ModuleId> {
        self.by_identity.get(identity).copied()
    }

    /// Append one resolved import edge to a module node.
    pub fn add_import(&mut self, module: ModuleId, import: ResolvedImport) {
        if let Some(node) = self.nodes.get_mut(module.0 as usize) {
            node.imports.push(import);
        }
    }

    /// Register one local declaration of `module` and return its symbol.
    ///
    /// Symbols are deterministic: the first declaration of a module is
    /// `SymbolId { module, index: 0 }`, the second `index: 1`, and so on in
    /// registration order. Public declarations are appended to the module's
    /// export table at the same time; imported bindings never enter it.
    ///
    /// Errors when the module already declares the same name (the parser
    /// normally reports this first with a source line) or when the name is
    /// already bound by an import.
    pub fn add_declaration(
        &mut self,
        module: ModuleId,
        name: &str,
        public: bool,
    ) -> Result<SymbolId, String> {
        let node = self.node_mut(module)?;
        if node.declarations.iter().any(|decl| decl.name == name) {
            return Err(format!(
                "duplicate local declaration '{name}' in module '{}'",
                node.source_name
            ));
        }
        if node
            .imported_bindings
            .iter()
            .any(|binding| binding.local_name == name)
        {
            return Err(format!(
                "local declaration '{name}' conflicts with an imported binding in module '{}'",
                node.source_name
            ));
        }
        let index = u32::try_from(node.declarations.len())
            .map_err(|_| format!("too many declarations in module '{}'", node.source_name))?;
        let symbol = SymbolId { module, index };
        node.declarations.push(DeclSymbol {
            symbol,
            name: name.to_string(),
            public,
        });
        if public {
            node.exports.push(ExportEntry {
                name: name.to_string(),
                symbol,
            });
        }
        Ok(symbol)
    }

    /// Record one binding introduced by an import edge of `module`.
    ///
    /// Imported bindings stay out of `declarations` and `exports`: nothing in
    /// the graph re-exports them implicitly. Binding the same local name from
    /// several modules is recorded as several bindings (the legacy pipeline
    /// merges such imports by name; milestone 4 resolves them by symbol), but
    /// a binding that clashes with a local declaration is rejected.
    pub fn add_imported_binding(
        &mut self,
        module: ModuleId,
        binding: ImportedBinding,
    ) -> Result<(), String> {
        let node = self.node_mut(module)?;
        if node
            .declarations
            .iter()
            .any(|decl| decl.name == binding.local_name)
        {
            return Err(format!(
                "imported binding '{}' conflicts with a local declaration in module '{}'",
                binding.local_name, node.source_name
            ));
        }
        node.imported_bindings.push(binding);
        Ok(())
    }

    pub fn declaration(&self, module: ModuleId, name: &str) -> Option<&DeclSymbol> {
        self.node(module)?
            .declarations
            .iter()
            .find(|decl| decl.name == name)
    }

    pub fn declaration_symbol(&self, module: ModuleId, name: &str) -> Option<SymbolId> {
        self.declaration(module, name).map(|decl| decl.symbol)
    }

    pub fn export(&self, module: ModuleId, name: &str) -> Option<&ExportEntry> {
        self.node(module)?
            .exports
            .iter()
            .find(|entry| entry.name == name)
    }

    pub fn symbol_for_export(&self, module: ModuleId, name: &str) -> Option<SymbolId> {
        self.export(module, name).map(|entry| entry.symbol)
    }

    pub fn imported_binding(&self, module: ModuleId, name: &str) -> Option<&ImportedBinding> {
        self.node(module)?
            .imported_bindings
            .iter()
            .find(|binding| binding.local_name == name)
    }

    fn node_mut(&mut self, module: ModuleId) -> Result<&mut ModuleNode, String> {
        self.nodes
            .get_mut(module.0 as usize)
            .ok_or_else(|| format!("unknown module id {} in module graph", module.0))
    }
}

fn is_valid_ident_segment(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
}

/// Convert a structured `use` path into a module specifier.
///
/// Replicates the legacy resolver's semantics from structured segments:
/// leading `self`/`super` qualifiers become `./`/`../` prefixes, a leading
/// `crate` is rejected, remaining segments join into a relative path, and the
/// result is normalized to a `.rss` specifier.
pub(super) fn use_path_to_spec(
    path: &Path,
    line: usize,
    segments: &[UsePathSegment],
) -> Result<String, SourcePathError> {
    let invalid = |message: &str| SourcePathError::InvalidImportSyntax {
        path: path.to_path_buf(),
        line,
        message: message.to_string(),
    };
    if segments.is_empty() {
        return Err(invalid("expected module path after 'use'"));
    }

    let mut path_prefix = PathBuf::new();
    let mut cursor = 0usize;
    let mut explicit_self = false;
    while cursor < segments.len() {
        match &segments[cursor] {
            UsePathSegment::Self_ => {
                explicit_self = true;
                cursor += 1;
            }
            UsePathSegment::Super => {
                path_prefix.push("..");
                cursor += 1;
            }
            UsePathSegment::Ident(name) if name == "crate" => {
                return Err(invalid(
                    "crate:: paths are not supported; use relative module paths",
                ));
            }
            UsePathSegment::Ident(_) => break,
        }
    }
    if cursor >= segments.len() {
        return Err(invalid("expected module name after path qualifiers"));
    }

    for segment in &segments[cursor..] {
        match segment {
            UsePathSegment::Ident(name) => {
                if !is_valid_ident_segment(name) {
                    return Err(invalid(&format!(
                        "invalid module path segment '{name}' in use directive"
                    )));
                }
                path_prefix.push(name);
            }
            // Mid-path qualifier words are literal file segments, mirroring
            // the legacy line-based resolver.
            UsePathSegment::Self_ => path_prefix.push("self"),
            UsePathSegment::Super => path_prefix.push("super"),
        }
    }

    let mut spec = path_prefix.to_string_lossy().replace('\\', "/");
    if spec.is_empty() {
        return Err(invalid("expected module path after 'use'"));
    }
    if explicit_self && !spec.starts_with("../") {
        spec = format!("./{spec}");
    }
    if !spec.ends_with(".rss") {
        spec.push_str(".rss");
    }
    Ok(spec)
}

/// Convert a joined `use` path spelling into a normalized module specifier,
/// applying the *same* leading self/super-qualifier and extension rules as
/// [`use_path_to_spec`].
///
/// The parser records a module namespace alias's path as the joined literal
/// spelling (`self::nested`, `super::shared`, `a::util`) in
/// [`ModuleNamespaceAlias::module_path`]. The semantic model re-resolves that
/// spelling to the imported module's source identity, so it must translate
/// leading qualifiers exactly like the loader's [`use_path_to_spec`]: a
/// leading `self` is a no-op (the module is relative to the current file),
/// each leading `super` becomes a `..` climb, and any later `self`/`super` is
/// a literal file segment. Sharing one routine keeps the loader and the
/// language-service resolver from drifting on these edge spellings.
///
/// Unlike [`use_path_to_spec`] this helper accepts the already-joined string,
/// so callers that only retained the spelling (rather than the structured
/// segments) get identical results without re-splitting logic.
pub fn use_path_string_to_spec(module_path: &str) -> String {
    let segments = module_path.split("::");
    let mut prefix = std::path::PathBuf::new();
    let mut iter = segments.clone();
    let mut explicit_self = false;
    // Leading qualifier words (`self`, `super`) translate like the structured
    // path; the first regular identifier ends the qualifier run.
    for segment in iter.by_ref() {
        match segment {
            "self" => explicit_self = true,
            "super" => prefix.push(".."),
            _ => {
                prefix.push(segment);
                break;
            }
        }
    }
    // Remaining segments are literal file path components (identity words
    // included), mirroring `use_path_to_spec`'s mid-path handling.
    for segment in iter {
        prefix.push(segment);
    }
    let mut spec = prefix.to_string_lossy().replace('\\', "/");
    if spec.is_empty() {
        // `self::` alone or an empty path has no module name; use_path_to_spec
        // would reject it. Keep parity by yielding `./` so the caller's
        // normalization still produces a deterministic (non-panicking) result;
        // real parser-produced aliases always carry a final module segment.
        spec = "./".to_string();
    }
    if explicit_self && !spec.starts_with("../") {
        spec = format!("./{spec}");
    }
    if !spec.ends_with(".rss") {
        spec.push_str(".rss");
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::{
        ImportTargetKind, ImportedBinding, ModuleGraph, ModuleId, ResolvedImport, SourceId,
        SymbolId, UsePathSegment, use_path_string_to_spec, use_path_to_spec,
    };
    use crate::compiler::source_loader::ImportClause;
    use crate::compiler::source_map::Span;
    use std::path::PathBuf;

    fn ident(name: &str) -> UsePathSegment {
        UsePathSegment::Ident(name.to_string())
    }

    #[test]
    fn use_path_to_spec_plain_module_path() {
        let path = PathBuf::from("/root/main.rss");
        let spec = use_path_to_spec(&path, 1, &[ident("helpers")]).expect("spec should resolve");
        assert_eq!(spec, "helpers.rss");
        let spec =
            use_path_to_spec(&path, 1, &[ident("a"), ident("b")]).expect("spec should resolve");
        assert_eq!(spec, "a/b.rss");
    }

    #[test]
    fn use_path_to_spec_self_and_super_qualifiers() {
        let path = PathBuf::from("/root/pkg/main.rss");
        let spec = use_path_to_spec(&path, 1, &[UsePathSegment::Self_, ident("nested")])
            .expect("spec should resolve");
        assert_eq!(spec, "./nested.rss");
        let spec = use_path_to_spec(&path, 1, &[UsePathSegment::Super, ident("shared")])
            .expect("spec should resolve");
        assert_eq!(spec, "../shared.rss");
        let spec = use_path_to_spec(
            &path,
            1,
            &[
                UsePathSegment::Self_,
                UsePathSegment::Super,
                ident("nested"),
            ],
        )
        .expect("spec should resolve");
        assert_eq!(spec, "../nested.rss");
        let spec = use_path_to_spec(
            &path,
            1,
            &[UsePathSegment::Self_, UsePathSegment::Self_, ident("x")],
        )
        .expect("spec should resolve");
        assert_eq!(spec, "./x.rss");
    }

    #[test]
    fn use_path_string_to_spec_matches_structured_resolution() {
        // The joined spelling (as recorded by the parser for a module
        // namespace alias) must resolve to the exact same spec as the
        // structured `use_path_to_spec` for the equivalent segment list.
        let path = PathBuf::from("/root/pkg/main.rss");
        let cases = [
            (vec![UsePathSegment::Self_, ident("nested")], "self::nested"),
            (
                vec![UsePathSegment::Super, ident("shared")],
                "super::shared",
            ),
            (
                vec![UsePathSegment::Ident("a".into()), ident("util")],
                "a::util",
            ),
            (
                vec![
                    UsePathSegment::Self_,
                    UsePathSegment::Super,
                    ident("nested"),
                ],
                "self::super::nested",
            ),
            (
                vec![UsePathSegment::Self_, UsePathSegment::Self_, ident("x")],
                "self::self::x",
            ),
            // A mid-path `super`/`self` word is a literal file segment, not a
            // qualifier; both resolve `a/self/b.rss`.
            (
                vec![ident("a"), UsePathSegment::Self_, ident("b")],
                "a::self::b",
            ),
        ];
        for (segments, spelling) in cases {
            let structured = use_path_to_spec(&path, 1, &segments).expect("structured spec");
            let from_spelling = use_path_string_to_spec(spelling);
            assert_eq!(
                from_spelling, structured,
                "spelling '{spelling}' must match structured {structured}"
            );
        }
    }

    #[test]
    fn use_path_to_spec_rejects_leading_crate() {
        let path = PathBuf::from("/root/main.rss");
        let err = use_path_to_spec(&path, 4, &[ident("crate"), ident("x")])
            .expect_err("crate:: should be rejected");
        let message = err.to_string();
        assert!(
            message.contains("crate:: paths are not supported"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("line 4"),
            "line should be preserved: {message}"
        );
    }

    #[test]
    fn use_path_to_spec_requires_module_name_after_qualifiers() {
        let path = PathBuf::from("/root/main.rss");
        let err = use_path_to_spec(&path, 2, &[UsePathSegment::Self_])
            .expect_err("bare self:: should be rejected");
        assert!(
            err.to_string()
                .contains("expected module name after path qualifiers"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn use_path_to_spec_mid_path_qualifier_words_are_literal_segments() {
        let path = PathBuf::from("/root/main.rss");
        let spec = use_path_to_spec(&path, 1, &[ident("a"), UsePathSegment::Self_, ident("b")])
            .expect("mid-path self should be a literal segment");
        assert_eq!(spec, "a/self/b.rss");
    }

    #[test]
    fn module_graph_ids_are_deterministic_and_deduplicated() {
        let mut graph = ModuleGraph::new();
        let root = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        assert_eq!(root, ModuleId(0));
        let again = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        assert_eq!(
            again, root,
            "re-registering the same identity reuses the node"
        );

        let nested = graph.add_node(
            PathBuf::from("/root/nested.rss"),
            "/root/nested.rss".to_string(),
            Vec::new(),
        );
        assert_eq!(nested, ModuleId(1));
        assert_eq!(graph.node(nested).expect("node exists").source, SourceId(1));
    }

    #[test]
    fn module_graph_same_stem_modules_are_distinct() {
        let mut graph = ModuleGraph::new();
        let first = graph.add_node(
            PathBuf::from("/root/a/common.rss"),
            "/root/a/common.rss".to_string(),
            Vec::new(),
        );
        let second = graph.add_node(
            PathBuf::from("/root/b/common.rss"),
            "/root/b/common.rss".to_string(),
            Vec::new(),
        );
        assert_ne!(
            first, second,
            "modules that share a stem but differ by directory must be distinct"
        );
        assert_eq!(graph.len(), 2);
        assert_eq!(
            graph.module_id_for_identity(PathBuf::from("/root/b/common.rss").as_path()),
            Some(second)
        );
    }

    #[test]
    fn module_graph_records_import_edges_and_targets() {
        let mut graph = ModuleGraph::new();
        let root = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        let nested = graph.add_node(
            PathBuf::from("/root/nested.rss"),
            "/root/nested.rss".to_string(),
            Vec::new(),
        );
        graph.add_import(
            root,
            ResolvedImport {
                kind: ImportTargetKind::FileModule,
                spec: "./nested.rss".to_string(),
                clause: ImportClause::Namespace("nested".to_string()),
                span: Span::new(0, 0, 0),
                line: 1,
                target: Some(nested),
            },
        );
        graph.add_import(
            root,
            ResolvedImport {
                kind: ImportTargetKind::BuiltinNamespace,
                spec: "json.rss".to_string(),
                clause: ImportClause::AllPublic,
                span: Span::new(0, 0, 0),
                line: 2,
                target: None,
            },
        );
        let node = graph.node(root).expect("root node exists");
        assert_eq!(node.imports.len(), 2);
        assert_eq!(node.imports[0].target, Some(nested));
        assert_eq!(node.imports[1].kind, ImportTargetKind::BuiltinNamespace);
        assert_eq!(node.imports[1].target, None);
    }

    #[test]
    fn symbol_ids_are_module_scoped() {
        let a = SymbolId {
            module: ModuleId(0),
            index: 3,
        };
        let b = SymbolId {
            module: ModuleId(1),
            index: 3,
        };
        assert_ne!(a, b, "same index in different modules must differ");
        assert_eq!(
            SymbolId {
                module: ModuleId(0),
                index: 3
            },
            a
        );
    }

    #[test]
    fn declaration_symbols_are_deterministic_and_module_owned() {
        let mut graph = ModuleGraph::new();
        let first = graph.add_node(
            PathBuf::from("/root/a.rss"),
            "/root/a.rss".to_string(),
            Vec::new(),
        );
        let second = graph.add_node(
            PathBuf::from("/root/b.rss"),
            "/root/b.rss".to_string(),
            Vec::new(),
        );
        let first_symbol = graph
            .add_declaration(first, "run", true)
            .expect("declaration registers");
        let second_symbol = graph
            .add_declaration(second, "run", true)
            .expect("declaration registers");
        assert_eq!(
            first_symbol,
            SymbolId {
                module: first,
                index: 0
            },
            "first declaration of a module owns symbol index 0"
        );
        assert_eq!(
            second_symbol,
            SymbolId {
                module: second,
                index: 0
            },
            "same index in a different module is a different symbol"
        );
        assert_ne!(first_symbol, second_symbol);

        let next = graph
            .add_declaration(first, "helper", false)
            .expect("declaration registers");
        assert_eq!(
            next,
            SymbolId {
                module: first,
                index: 1
            },
            "symbols are assigned in declaration order"
        );
        assert_eq!(graph.declaration_symbol(first, "run"), Some(first_symbol));
        assert_eq!(graph.declaration_symbol(second, "run"), Some(second_symbol));
    }

    #[test]
    fn export_table_contains_only_public_declarations() {
        let mut graph = ModuleGraph::new();
        let module = graph.add_node(
            PathBuf::from("/root/lib.rss"),
            "/root/lib.rss".to_string(),
            Vec::new(),
        );
        let public = graph
            .add_declaration(module, "visible", true)
            .expect("public declaration registers");
        graph
            .add_declaration(module, "hidden", false)
            .expect("private declaration registers");

        let node = graph.node(module).expect("node exists");
        assert_eq!(
            node.exports.len(),
            1,
            "only the pub declaration is exported"
        );
        assert_eq!(node.exports[0].name, "visible");
        assert_eq!(node.exports[0].symbol, public);
        assert_eq!(graph.symbol_for_export(module, "visible"), Some(public));
        assert_eq!(
            graph.symbol_for_export(module, "hidden"),
            None,
            "private declarations never enter the export table"
        );
        assert!(
            !graph
                .declaration(module, "hidden")
                .expect("declaration exists")
                .public
        );
        assert_eq!(node.declarations.len(), 2);
    }

    #[test]
    fn duplicate_local_declaration_is_rejected() {
        let mut graph = ModuleGraph::new();
        let module = graph.add_node(
            PathBuf::from("/root/lib.rss"),
            "/root/lib.rss".to_string(),
            Vec::new(),
        );
        graph
            .add_declaration(module, "run", true)
            .expect("first declaration registers");
        let err = graph
            .add_declaration(module, "run", false)
            .expect_err("second same-named declaration must be rejected");
        assert!(
            err.contains("duplicate local declaration 'run'"),
            "unexpected error: {err}"
        );
        // The same name in another module is not a duplicate.
        let other = graph.add_node(
            PathBuf::from("/root/other.rss"),
            "/root/other.rss".to_string(),
            Vec::new(),
        );
        graph
            .add_declaration(other, "run", true)
            .expect("independent module may reuse the name");
    }

    #[test]
    fn imported_bindings_stay_separate_from_local_declarations() {
        let mut graph = ModuleGraph::new();
        let importer = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        let source = graph.add_node(
            PathBuf::from("/root/util.rss"),
            "/root/util.rss".to_string(),
            Vec::new(),
        );
        let exported = graph
            .add_declaration(source, "helper", true)
            .expect("source export registers");
        graph
            .add_imported_binding(
                importer,
                ImportedBinding {
                    local_name: "helper".to_string(),
                    source_module: source,
                    source_symbol: exported,
                    source_name: "helper".to_string(),
                },
            )
            .expect("imported binding registers");

        let node = graph.node(importer).expect("importer node exists");
        assert_eq!(
            node.declarations.len(),
            0,
            "imported symbols are not local declarations"
        );
        assert_eq!(
            node.exports.len(),
            0,
            "imported symbols never enter the export table"
        );
        assert_eq!(node.imported_bindings.len(), 1);
        let binding = graph
            .imported_binding(importer, "helper")
            .expect("binding exists");
        assert_eq!(binding.source_module, source);
        assert_eq!(binding.source_symbol, exported);
        assert_eq!(binding.source_name, "helper");
    }

    #[test]
    fn same_named_declarations_across_modules_coexist() {
        let mut graph = ModuleGraph::new();
        let first = graph.add_node(
            PathBuf::from("/root/a/util.rss"),
            "/root/a/util.rss".to_string(),
            Vec::new(),
        );
        let second = graph.add_node(
            PathBuf::from("/root/b/util.rss"),
            "/root/b/util.rss".to_string(),
            Vec::new(),
        );
        let first_helper = graph
            .add_declaration(first, "helper", false)
            .expect("private helper registers");
        let second_helper = graph
            .add_declaration(second, "helper", false)
            .expect("private helper registers");
        assert_ne!(
            first_helper, second_helper,
            "same-named helpers in independent modules have distinct symbols"
        );
        assert_eq!(
            graph.declaration_symbol(first, "helper"),
            Some(first_helper)
        );
        assert_eq!(
            graph.declaration_symbol(second, "helper"),
            Some(second_helper)
        );

        // Same-named *public* functions in independent modules coexist too,
        // each in its own export table.
        graph
            .add_declaration(first, "run", true)
            .expect("public run registers in first module");
        graph
            .add_declaration(second, "run", true)
            .expect("public run registers in second module");
        let first_run = graph
            .symbol_for_export(first, "run")
            .expect("first export exists");
        let second_run = graph
            .symbol_for_export(second, "run")
            .expect("second export exists");
        assert_ne!(first_run, second_run);
    }

    #[test]
    fn no_implicit_transitive_reexport_through_the_graph() {
        let mut graph = ModuleGraph::new();
        let root = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        let middle = graph.add_node(
            PathBuf::from("/root/middle.rss"),
            "/root/middle.rss".to_string(),
            Vec::new(),
        );
        let leaf = graph.add_node(
            PathBuf::from("/root/leaf.rss"),
            "/root/leaf.rss".to_string(),
            Vec::new(),
        );
        let shared = graph
            .add_declaration(leaf, "shared", true)
            .expect("leaf export registers");
        let middle_own = graph
            .add_declaration(middle, "middle_own", true)
            .expect("middle export registers");

        // middle imports leaf's export; root imports middle's export.
        for (importer, source, source_symbol, source_name) in [
            (middle, leaf, shared, "shared"),
            (root, middle, middle_own, "middle_own"),
        ] {
            graph
                .add_imported_binding(
                    importer,
                    ImportedBinding {
                        local_name: source_name.to_string(),
                        source_module: source,
                        source_symbol,
                        source_name: source_name.to_string(),
                    },
                )
                .expect("imported binding registers");
        }

        assert_eq!(
            graph.symbol_for_export(middle, "shared"),
            None,
            "middle's export table must not re-export leaf's function"
        );
        assert_eq!(
            graph.symbol_for_export(root, "shared"),
            None,
            "root's export table must not see leaf's function through middle"
        );
        assert_eq!(
            graph.symbol_for_export(middle, "middle_own"),
            Some(middle_own),
            "middle's own public declaration stays in its export table"
        );
        assert_eq!(
            graph.symbol_for_export(root, "middle_own"),
            None,
            "root's export table is empty: direct imports become bindings, not exports"
        );
        assert!(graph.imported_binding(root, "middle_own").is_some());
        assert!(graph.imported_binding(middle, "shared").is_some());
        assert!(
            graph
                .node(root)
                .expect("root node exists")
                .exports
                .is_empty()
        );
    }

    #[test]
    fn imported_binding_clashing_with_local_declaration_is_rejected() {
        let mut graph = ModuleGraph::new();
        let importer = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        let source = graph.add_node(
            PathBuf::from("/root/util.rss"),
            "/root/util.rss".to_string(),
            Vec::new(),
        );
        let exported = graph
            .add_declaration(source, "helper", true)
            .expect("source export registers");
        graph
            .add_declaration(importer, "helper", true)
            .expect("local declaration registers");

        let err = graph
            .add_imported_binding(
                importer,
                ImportedBinding {
                    local_name: "helper".to_string(),
                    source_module: source,
                    source_symbol: exported,
                    source_name: "helper".to_string(),
                },
            )
            .expect_err("binding a name already declared locally must be rejected");
        assert!(
            err.contains("conflicts with a local declaration"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn duplicate_imported_bindings_are_recorded_like_the_legacy_merge() {
        // Two modules exporting the same name, both imported into one module:
        // the legacy pipeline merges such imports by name, so the graph keeps
        // every binding instead of rejecting the second one.
        let mut graph = ModuleGraph::new();
        let importer = graph.add_node(
            PathBuf::from("/root/main.rss"),
            "/root/main.rss".to_string(),
            Vec::new(),
        );
        let first = graph.add_node(
            PathBuf::from("/root/a/util.rss"),
            "/root/a/util.rss".to_string(),
            Vec::new(),
        );
        let second = graph.add_node(
            PathBuf::from("/root/b/util.rss"),
            "/root/b/util.rss".to_string(),
            Vec::new(),
        );
        let first_helper = graph
            .add_declaration(first, "helper", true)
            .expect("first export registers");
        let second_helper = graph
            .add_declaration(second, "helper", true)
            .expect("second export registers");
        graph
            .add_imported_binding(
                importer,
                ImportedBinding {
                    local_name: "helper".to_string(),
                    source_module: first,
                    source_symbol: first_helper,
                    source_name: "helper".to_string(),
                },
            )
            .expect("first binding registers");
        graph
            .add_imported_binding(
                importer,
                ImportedBinding {
                    local_name: "helper".to_string(),
                    source_module: second,
                    source_symbol: second_helper,
                    source_name: "helper".to_string(),
                },
            )
            .expect("second binding with the same local name registers");

        let node = graph.node(importer).expect("importer node exists");
        assert_eq!(node.imported_bindings.len(), 2);
        assert_eq!(node.imported_bindings[0].source_module, first);
        assert_eq!(node.imported_bindings[1].source_module, second);
        assert_eq!(
            node.declarations.len(),
            0,
            "bindings are never declarations"
        );
        assert!(node.exports.is_empty(), "bindings are never exports");
    }
}
