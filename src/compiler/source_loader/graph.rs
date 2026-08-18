use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::compiler::source_map::SourceMap;

use super::super::{
    CompileSourceFileOptions, ParseError, SourceError, SourceFlavor, SourcePathError, frontends,
    ir::{Expr, FrontendIr, FunctionDecl, Stmt, TypeSchema},
    linker::{ParsedUnit, module_scope_prefix},
    modules::{ImportTargetKind, ImportedBinding, ModuleGraph, ModuleId, ResolvedImport, SymbolId},
};
use super::imports::{
    is_builtin_host_namespace_spec, is_module_specifier, is_virtual_host_namespace_spec,
    parse_module_imports, resolve_module_path, scan_module_imports,
    should_treat_missing_module_as_host_namespace,
};
use super::model::{ExportedFunctionSignature, ImportClause, ModuleCollectState, ModuleImport};

pub(super) fn collect_module_units(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    state: &mut ModuleCollectState,
) -> Result<(), SourcePathError> {
    // Register this module in the semantic graph. Registration is
    // identity-keyed and idempotent, so the root and every nested module get
    // a deterministic `ModuleId`/`SourceId` in first-encounter order.
    let current_id =
        state
            .module_graph
            .add_node(path.to_path_buf(), path.display().to_string(), Vec::new());
    // Register the module's raw text in the compilation-wide source map at
    // its graph `SourceId`, before any scan or parse that can produce spans
    // referencing that id (milestone 5: every span stays owned by its
    // module's source).
    let current_source_id = state
        .module_graph
        .node(current_id)
        .map(|node| node.source)
        .unwrap_or(crate::compiler::modules::SourceId(0));
    state.sources.add_source_at(
        current_source_id.0,
        path.display().to_string(),
        source.to_string(),
    );
    let (imports, decls) = scan_module_imports(source, flavor, path, options).map_err(|err| {
        // Nested module sources surface their parse errors through the same
        // path-prefixed diagnostic shape the compile parse uses. The root is
        // scanned (and fails, if at all) in `load_units_for_source_file`
        // before this point, so it never receives a prefix here. The scan
        // parser numbers spans with its own local source id 0, so the span
        // is always rebuilt against the owning module's graph source id —
        // offsets from one module must never be interpreted in another.
        match err {
            SourcePathError::Source(SourceError::Parse(mut parse)) => {
                parse.message = format!("{}: {}", path.display(), parse.message);
                parse.span = None;
                parse = parse.with_line_span_from_source(&state.sources, current_source_id.0);
                SourcePathError::Source(SourceError::Parse(parse))
            }
            other => other,
        }
    })?;
    for (import_index, import) in imports.iter().enumerate() {
        let spec = import.spec.clone();
        let span = decls
            .get(import_index)
            .map(|decl| decl.span)
            .unwrap_or_else(|| crate::compiler::source_map::Span::new(0, 0, 0));
        if is_builtin_host_namespace_spec(&spec) {
            state.module_graph.add_import(
                current_id,
                ResolvedImport {
                    kind: ImportTargetKind::BuiltinNamespace,
                    spec,
                    clause: import.clause.clone(),
                    span,
                    line: import.line,
                    target: None,
                },
            );
            continue;
        }
        if !is_module_specifier(&spec) {
            // Plugin-managed host imports (non-RustScript flavors) stay on
            // their dedicated resolution path.
            state.module_graph.add_import(
                current_id,
                ResolvedImport {
                    kind: ImportTargetKind::HostNamespace,
                    spec,
                    clause: import.clause.clone(),
                    span,
                    line: import.line,
                    target: None,
                },
            );
            continue;
        }
        let resolved = resolve_module_path(path, &spec, options)?;
        let key = resolved.clone();
        if key == path && is_virtual_host_namespace_spec(&spec, options) {
            // `use io;` / `use re;` inside files named `io.rss` / `re.rss` should
            // keep behaving as host-namespace imports instead of self-module cycles.
            state.module_graph.add_import(
                current_id,
                ResolvedImport {
                    kind: ImportTargetKind::HostNamespace,
                    spec,
                    clause: import.clause.clone(),
                    span,
                    line: import.line,
                    target: None,
                },
            );
            continue;
        }
        if state.visiting.contains(&key) {
            return Err(SourcePathError::ImportCycle(key));
        }
        if state.seen.contains(&key) {
            // Already loaded: keep the resolved edge pointing at the existing
            // node instead of re-collecting the module.
            let target = state.module_graph.module_id_for_identity(&key);
            state.module_graph.add_import(
                current_id,
                ResolvedImport {
                    kind: ImportTargetKind::FileModule,
                    spec,
                    clause: import.clause.clone(),
                    span,
                    line: import.line,
                    target,
                },
            );
            continue;
        }

        let module_source_raw =
            if let Some(source) = module_source_override(options, &spec, &resolved) {
                source.to_string()
            } else {
                match std::fs::read_to_string(&resolved) {
                    Ok(source) => source,
                    Err(err) => {
                        if should_treat_missing_module_as_host_namespace(&spec, options, &err) {
                            state.module_graph.add_import(
                                current_id,
                                ResolvedImport {
                                    kind: ImportTargetKind::HostNamespace,
                                    spec,
                                    clause: import.clause.clone(),
                                    span,
                                    line: import.line,
                                    target: None,
                                },
                            );
                            continue;
                        }
                        return Err(SourcePathError::Io(err));
                    }
                }
            };
        state.visiting.push(key.clone());
        collect_module_units(
            &resolved,
            &module_source_raw,
            SourceFlavor::RustScript,
            options,
            state,
        )?;
        state.visiting.pop();

        let module_imports = parse_module_imports(
            &module_source_raw,
            SourceFlavor::RustScript,
            &resolved,
            options,
        )?;
        let module_source_id = state
            .module_graph
            .module_id_for_identity(&key)
            .and_then(|module| state.module_graph.node(module))
            .map(|node| node.source.0)
            .unwrap_or(0);
        let mut parsed = frontends::parse_module_source_with_source_id(
            &module_source_raw,
            SourceFlavor::RustScript,
            options,
            module_source_id,
        )
        .map_err(|mut err| {
            // Nested module sources are parsed verbatim (no synthetic
            // prelude, no textual rewrite), so the parse already reports the
            // owning module's lines; rebuild the span against the
            // compilation-wide map and prefix the module path.
            err.span = None;
            let mut parse = err.with_line_span_from_source(&state.sources, module_source_id);
            parse.message = format!("{}: {}", resolved.display(), parse.message);
            SourceError::Parse(parse)
        })?;
        let extern_names = parsed
            .implicit_extern_names
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let exports = parsed
            .functions
            .iter()
            .filter(|func| func.exported && !extern_names.contains(func.name.as_str()))
            .map(|func| {
                (
                    func.name.clone(),
                    ExportedFunctionSignature {
                        arity: func.arity,
                        type_params: func.type_params.clone(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let target = state
            .module_graph
            .module_id_for_identity(&key)
            .expect("module node should be registered during collection");
        record_module_symbols(
            state,
            target,
            &resolved,
            &module_imports,
            &mut parsed,
            options,
        )?;
        state.units.push(ParsedUnit {
            parsed,
            scope_identity: Some(module_scope_prefix(&resolved, target)),
            source_name: resolved.display().to_string(),
            module: target,
            source_id: module_source_id,
        });
        state.module_graph.add_import(
            current_id,
            ResolvedImport {
                kind: ImportTargetKind::FileModule,
                spec,
                clause: import.clause.clone(),
                span,
                line: import.line,
                target: Some(target),
            },
        );
        state.module_exports.insert(key.clone(), exports);
        state.seen.insert(key);
    }
    Ok(())
}

fn module_source_override<'a>(
    options: &'a CompileSourceFileOptions,
    spec: &str,
    resolved_path: &Path,
) -> Option<&'a str> {
    options.module_override_source(spec).or_else(|| {
        options.module_override_source(&resolved_path.to_string_lossy().replace('\\', "/"))
    })
}

/// Build the exported-signature table keyed by [`SymbolId`].
///
/// The loader validates call sites against the exported arity and type
/// parameters at resolution time (the parse can no longer see them: module
/// sources are parsed verbatim without a synthetic prelude).
fn exported_signature_table(
    state: &ModuleCollectState,
    graph: &ModuleGraph,
) -> HashMap<SymbolId, ExportedFunctionSignature> {
    let mut table = HashMap::new();
    for node in graph.nodes() {
        let Some(exports) = state.module_exports.get(&node.identity) else {
            continue;
        };
        for entry in &node.exports {
            if let Some(signature) = exports.get(&entry.name) {
                table.insert(entry.symbol, signature.clone());
            }
        }
    }
    table
}

/// Namespace portion of a qualified call name (`au::helper` → `au`).
fn namespace_of(qualified: &str) -> &str {
    qualified
        .split_once("::")
        .map(|(namespace, _)| namespace)
        .unwrap_or(qualified)
}

/// Clause-derived namespace alias of one import edge, mirroring the parser's
/// module-namespace alias rules: the `as` alias for namespace imports, the
/// spec stem for all-public imports, and no namespace for named imports.
fn namespace_alias_for_import(import: &ResolvedImport) -> Option<String> {
    match &import.clause {
        ImportClause::Namespace(alias) => Some(alias.clone()),
        ImportClause::AllPublic => Path::new(&import.spec)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.to_string()),
        ImportClause::Named(_) | ImportClause::Prefix(_) => None,
    }
}

/// File-module import targets that bind `namespace`, either through a clause
/// alias (`use a::util as au;` binds `au`) or through the spec stem
/// (host-form single-segment imports such as `use module;` whose namespace
/// the parser resolved as a host root).
fn file_module_targets_for_namespace(
    graph: &ModuleGraph,
    module: ModuleId,
    namespace: &str,
) -> Vec<ModuleId> {
    let Some(node) = graph.node(module) else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for import in &node.imports {
        if import.kind != ImportTargetKind::FileModule {
            continue;
        }
        let Some(target) = import.target else {
            continue;
        };
        let stem = Path::new(&import.spec)
            .file_stem()
            .and_then(|stem| stem.to_str());
        if (namespace_alias_for_import(import).as_deref() == Some(namespace)
            || stem == Some(namespace))
            && !targets.contains(&target)
        {
            targets.push(target);
        }
    }
    targets
}

/// Whether any file-module import edge of `module` binds `qualified`'s
/// namespace. Host-form declarations whose namespace names a file module are
/// kept out of the module's declaration table; the resolution pass converts
/// their call sites to [`Expr::ModuleCall`] instead.
fn namespace_has_file_module_target(
    graph: &ModuleGraph,
    module: ModuleId,
    qualified: &str,
) -> bool {
    !file_module_targets_for_namespace(graph, module, namespace_of(qualified)).is_empty()
}

/// One function binding introduced by an import edge, before it is recorded
/// in the module graph.
struct ImportBindingData {
    /// Name the importing module binds (`as` alias for named imports).
    local_name: String,
    /// Name of the declaration in the source module.
    source_name: String,
    /// Source module once its graph node is known; `None` for host/builtin
    /// namespaces that stay on their dedicated resolution paths.
    source_module: Option<ModuleId>,
    /// Line of the `use` directive that introduced the binding.
    line: usize,
}

/// Collect the function bindings a module's imports introduce, structurally.
///
/// Mirrors the legacy `collect_imported_module_functions` resolution rules
/// (builtin and non-module specifiers skipped, missing virtual host namespaces
/// tolerated) but preserves `as` aliases and resolves the source module in
/// the semantic graph, so the loader can record [`ImportedBinding`]s that
/// stay separate from local declarations.
fn collect_imported_bindings(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, ExportedFunctionSignature>>,
    graph: &ModuleGraph,
    options: &CompileSourceFileOptions,
) -> Result<Vec<ImportBindingData>, SourcePathError> {
    let mut bindings = Vec::new();
    for import in imports {
        if is_builtin_host_namespace_spec(&import.spec) {
            continue;
        }
        if !is_module_specifier(&import.spec) {
            continue;
        }

        let resolved = resolve_module_path(path, &import.spec, options)?;
        let Some(exports) = module_exports.get(&resolved) else {
            if is_virtual_host_namespace_spec(&import.spec, options) {
                continue;
            }
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line: import.line,
                message: format!("module '{}' did not load", import.spec),
            });
        };
        let source_module = graph.module_id_for_identity(&resolved);

        match &import.clause {
            ImportClause::AllPublic | ImportClause::Namespace(_) | ImportClause::Prefix(_) => {
                for name in exports.keys() {
                    bindings.push(ImportBindingData {
                        local_name: name.clone(),
                        source_name: name.clone(),
                        source_module,
                        line: import.line,
                    });
                }
            }
            ImportClause::Named(named) => {
                for binding in named {
                    let _signature = exports.get(&binding.imported).cloned().ok_or_else(|| {
                        SourcePathError::InvalidImportSyntax {
                            path: path.to_path_buf(),
                            line: import.line,
                            message: format!(
                                "module '{}' has no public function '{}'",
                                import.spec, binding.imported
                            ),
                        }
                    })?;
                    bindings.push(ImportBindingData {
                        local_name: binding.local.clone(),
                        source_name: binding.imported.clone(),
                        source_module,
                        line: import.line,
                    });
                }
            }
        }
    }
    Ok(bindings)
}

/// Attach milestone-3 declaration symbols and imported bindings to a parsed
/// unit's module node, then resolve imported call sites to their target
/// symbols (milestone 4).
///
/// Runs once per module, right after its unit is parsed: imported-binding
/// mirror declarations and implicit externs are skipped, every remaining
/// function declaration receives a [`SymbolId`] owned by the module, public
/// declarations populate the module's export table, and every import-introduced
/// binding is recorded separately in the module's imported-binding table.
/// The same `symbol` is written back onto the parsed [`FunctionDecl`] so the
/// linker can collect it through `merge_units`. Finally, calls to imported
/// functions and module namespace members are resolved to [`Expr::ModuleCall`]
/// nodes carrying the target [`SymbolId`].
pub(super) fn record_module_symbols(
    state: &mut ModuleCollectState,
    module: ModuleId,
    path: &Path,
    imports: &[ModuleImport],
    parsed: &mut FrontendIr,
    options: &CompileSourceFileOptions,
) -> Result<(), SourcePathError> {
    let bindings = collect_imported_bindings(
        path,
        imports,
        &state.module_exports,
        &state.module_graph,
        options,
    )?;
    // Implicit externs (module mode) mirror calls the loader must resolve or
    // reject; they never become local declarations or flat entries.
    let extern_names = parsed
        .implicit_extern_names
        .iter()
        .cloned()
        .collect::<HashSet<_>>();

    let module_source_id = state
        .module_graph
        .node(module)
        .map(|node| node.source.0)
        .unwrap_or(0);
    let decl_lines = collect_function_decl_lines(&parsed.stmts);
    let signatures = exported_signature_table(state, &state.module_graph);

    for func in &mut parsed.functions {
        if extern_names.contains(func.name.as_str()) {
            // Implicit extern (module mode): the resolution pass resolves
            // (or rejects) its call sites; never a local decl.
            continue;
        }
        if func.name.contains("::")
            && namespace_has_file_module_target(&state.module_graph, module, &func.name)
        {
            // Host-form declaration whose namespace names a file module
            // (single-segment import forms such as `use module;`): the
            // resolution pass converts its call sites to `ModuleCall`, so
            // the declaration must not become a flat host entry.
            continue;
        }
        // A local declaration whose name is also imported is recorded here
        // and then rejected when the import binding is added below: no
        // silent shadowing of imported names.
        let decl_line = decl_lines
            .get(&func.index)
            .copied()
            .map(|line| line as usize)
            .unwrap_or(1);
        let symbol = state
            .module_graph
            .add_declaration(module, &func.name, func.exported)
            .map_err(|message| {
                // The duplicate symbol diagnostic renders from the owning
                // module source: same-named declarations collide inside one
                // module only, and the span points at the redeclaration.
                let span = state.sources.line_span(module_source_id, decl_line);
                SourcePathError::Source(SourceError::Parse(ParseError {
                    span,
                    code: None,
                    line: decl_line,
                    message: format!("{}: {message}", path.display()),
                }))
            })?;
        func.symbol = Some(symbol);
    }

    for binding in bindings {
        let Some(source_module) = binding.source_module else {
            continue;
        };
        let binding_line = binding.line.max(1);
        let source_symbol = state
            .module_graph
            .symbol_for_export(source_module, &binding.source_name)
            .ok_or_else(|| {
                // Visibility failure: the import directive is the offending
                // site, so the span points at the `use` line in the
                // importing module's source.
                let span = state.sources.line_span(module_source_id, binding_line);
                SourcePathError::Source(SourceError::Parse(ParseError {
                    span,
                    code: None,
                    line: binding_line,
                    message: format!(
                        "{}: imported function '{}' is not exported by module {}",
                        path.display(),
                        binding.source_name,
                        source_module.0
                    ),
                }))
            })?;
        state
            .module_graph
            .add_imported_binding(
                module,
                ImportedBinding {
                    local_name: binding.local_name,
                    source_module,
                    source_symbol,
                    source_name: binding.source_name,
                },
            )
            .map_err(|message| {
                let span = state.sources.line_span(module_source_id, binding_line);
                SourcePathError::Source(SourceError::Parse(ParseError {
                    span,
                    code: None,
                    line: binding_line,
                    message: format!("{}: {message}", path.display()),
                }))
            })?;
    }

    resolve_imported_call_sites(
        module,
        path,
        &state.module_graph,
        &state.sources,
        &signatures,
        &extern_names,
        parsed,
    )
}

fn collect_function_decl_lines(stmts: &[Stmt]) -> HashMap<u16, u32> {
    let mut lines = HashMap::new();
    record_function_decl_lines(stmts, &mut lines);
    lines
}

fn record_function_decl_lines(stmts: &[Stmt], lines: &mut HashMap<u16, u32>) {
    for stmt in stmts {
        match stmt {
            Stmt::FuncDecl { index, line, .. } => {
                lines.entry(*index).or_insert(*line);
            }
            Stmt::IfElse {
                then_branch,
                else_branch,
                ..
            } => {
                record_function_decl_lines(then_branch, lines);
                record_function_decl_lines(else_branch, lines);
            }
            Stmt::For {
                init, post, body, ..
            } => {
                record_function_decl_lines(std::slice::from_ref(init.as_ref()), lines);
                record_function_decl_lines(std::slice::from_ref(post.as_ref()), lines);
                record_function_decl_lines(body, lines);
            }
            Stmt::While { body, .. } => record_function_decl_lines(body, lines),
            _ => {}
        }
    }
}

/// Resolution context for one module's imported-call pass.
struct CallResolutionContext<'a> {
    functions_by_index: HashMap<u16, &'a FunctionDecl>,
    /// Direct call names bound by exactly one source module (keyed by the
    /// name the importing module binds: `as` alias or source name).
    plain_symbols: HashMap<String, SymbolId>,
    /// Direct call names bound from several modules with different symbols.
    ambiguous_names: HashSet<String>,
    /// Exported arity/type-parameter table for signature validation.
    signatures: &'a HashMap<SymbolId, ExportedFunctionSignature>,
    /// Implicit-extern names produced by the parser (module mode).
    extern_names: &'a HashSet<String>,
    module: ModuleId,
    path: &'a Path,
    graph: &'a ModuleGraph,
    sources: &'a SourceMap,
    source_id: u32,
}

impl<'a> CallResolutionContext<'a> {
    /// Resolve one unit-local call name to its target symbol.
    ///
    /// Names with a namespace separator resolve through the module's
    /// file-module import edges (clause alias or spec stem); plain names
    /// resolve through the imported-binding table. Returns `Ok(None)` for
    /// names that are neither (the caller decides how to report them).
    fn target_for_call(
        &self,
        decl_name: &str,
        arg_count: usize,
        type_args: &[TypeSchema],
        line: u32,
    ) -> Result<Option<SymbolId>, SourcePathError> {
        if let Some((namespace, member)) = decl_name.split_once("::") {
            return self.target_for_namespace_call(
                namespace, member, decl_name, arg_count, type_args, line,
            );
        }
        if self.ambiguous_names.contains(decl_name) {
            return Err(ambiguous_imported_call_error(
                self.path,
                decl_name,
                self.sources,
                self.source_id,
                line,
            ));
        }
        if let Some(symbol) = self.plain_symbols.get(decl_name) {
            self.validate_imported_signature(decl_name, *symbol, arg_count, type_args, line)?;
            return Ok(Some(*symbol));
        }
        Err(unknown_function_error(
            self.path,
            decl_name,
            self.sources,
            self.source_id,
            line,
        ))
    }

    fn target_for_namespace_call(
        &self,
        namespace: &str,
        member: &str,
        qualified: &str,
        arg_count: usize,
        type_args: &[TypeSchema],
        line: u32,
    ) -> Result<Option<SymbolId>, SourcePathError> {
        if member.contains("::") {
            // Multi-level module member paths are not supported; the legacy
            // pipeline reported the same call as an unknown namespace call.
            return Err(unknown_namespace_call_error(
                self.path,
                qualified,
                self.sources,
                self.source_id,
                line,
            ));
        }
        let mut found = HashSet::new();
        for target in file_module_targets_for_namespace(self.graph, self.module, namespace) {
            if let Some(symbol) = self.graph.symbol_for_export(target, member) {
                found.insert(symbol);
            }
        }
        match found.len() {
            0 => {
                if self.extern_names.contains(qualified) {
                    // Multi-segment import form whose namespace or member did
                    // not resolve to a public export.
                    Err(unknown_namespace_call_error(
                        self.path,
                        qualified,
                        self.sources,
                        self.source_id,
                        line,
                    ))
                } else {
                    // Host-form declaration through a file-module namespace
                    // whose module does not export the member: report like
                    // the legacy parse did for the unqualified name.
                    Err(unknown_function_error(
                        self.path,
                        member,
                        self.sources,
                        self.source_id,
                        line,
                    ))
                }
            }
            1 => {
                let symbol = found.into_iter().next().expect("exactly one symbol");
                self.validate_imported_signature(qualified, symbol, arg_count, type_args, line)?;
                Ok(Some(symbol))
            }
            _ => Err(ambiguous_imported_call_error(
                self.path,
                qualified,
                self.sources,
                self.source_id,
                line,
            )),
        }
    }

    /// Validate a resolved call against the exported arity and type
    /// parameters, mirroring the messages the synthetic prelude used to
    /// produce at parse time.
    fn validate_imported_signature(
        &self,
        call_name: &str,
        symbol: SymbolId,
        arg_count: usize,
        type_args: &[TypeSchema],
        line: u32,
    ) -> Result<(), SourcePathError> {
        let Some(signature) = self.signatures.get(&symbol) else {
            return Ok(());
        };
        let parse_error = |message: String| {
            SourcePathError::Source(SourceError::Parse(ParseError {
                span: self.sources.line_span(self.source_id, line as usize),
                code: None,
                line: line as usize,
                message: format!("{}: {message}", self.path.display()),
            }))
        };
        if usize::from(signature.arity) != arg_count {
            return Err(parse_error(format!(
                "function '{call_name}' expects {} arguments",
                signature.arity
            )));
        }
        if signature.type_params.is_empty() {
            if type_args.is_empty() {
                return Ok(());
            }
            return Err(parse_error(format!(
                "function '{call_name}' does not accept explicit type arguments"
            )));
        }
        if signature.type_params.len() != type_args.len() {
            return Err(parse_error(format!(
                "function '{call_name}' expects {} type arguments, got {}",
                signature.type_params.len(),
                type_args.len()
            )));
        }
        Ok(())
    }

    /// Resolve one function-value reference to its target symbol.
    fn target_for_function_ref(
        &self,
        name: &str,
        line: u32,
    ) -> Result<Option<SymbolId>, SourcePathError> {
        if self.ambiguous_names.contains(name) {
            return Err(ambiguous_imported_call_error(
                self.path,
                name,
                self.sources,
                self.source_id,
                line,
            ));
        }
        if let Some(symbol) = self.plain_symbols.get(name) {
            return Ok(Some(*symbol));
        }
        Err(SourcePathError::Source(SourceError::Parse(ParseError {
            span: self.sources.line_span(self.source_id, line as usize),
            code: None,
            line: line as usize,
            message: format!("{}: unknown local '{}'", self.path.display(), name),
        })))
    }
}

/// Resolve every call to an imported function to its compiler-owned
/// [`SymbolId`] before unit merge.
///
/// Module sources are parsed verbatim (no synthetic prelude, no textual
/// rewrite), so call sites reach this pass in the shapes the parser produced:
///
/// - Direct calls (`helper(...)`) parse as implicit externs. A name bound
///   from exactly one source module maps to that module's symbol; a name
///   bound from several modules is ambiguous and becomes a diagnostic; an
///   unbound name is rejected as an unknown function.
/// - Namespace calls (`au::helper(...)`) parse either as implicit externs
///   carrying the qualified name (multi-segment import forms) or as
///   host-form calls whose namespace the parser treated as a host root
///   (single-segment import forms such as `use module;`). Both resolve
///   through the module's file-module import edges: the clause alias or the
///   spec stem maps the namespace to its target module, and the member must
///   be one of its public exports.
/// - Function values (`let f = helper;`) parse as
///   [`Expr::UnresolvedFunctionRef`] and resolve to
///   [`Expr::ModuleFunctionRef`].
///
/// Local calls (declarations that own a symbol) and host/builtin calls are
/// left untouched; the linker remaps them by symbol or keeps their reserved
/// builtin index.
fn resolve_imported_call_sites(
    module: ModuleId,
    path: &Path,
    graph: &ModuleGraph,
    sources: &SourceMap,
    signatures: &HashMap<SymbolId, ExportedFunctionSignature>,
    extern_names: &HashSet<String>,
    parsed: &mut FrontendIr,
) -> Result<(), SourcePathError> {
    let source_id = graph.node(module).map(|node| node.source.0).unwrap_or(0);
    let mut plain_symbols = HashMap::<String, SymbolId>::new();
    let mut ambiguous_names = HashSet::<String>::new();
    if let Some(node) = graph.node(module) {
        for binding in &node.imported_bindings {
            match plain_symbols.entry(binding.local_name.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(binding.source_symbol);
                }
                std::collections::hash_map::Entry::Occupied(entry)
                    if *entry.get() != binding.source_symbol =>
                {
                    ambiguous_names.insert(binding.local_name.clone());
                }
                std::collections::hash_map::Entry::Occupied(_) => {}
            }
        }
    }

    let functions_by_index = parsed
        .functions
        .iter()
        .map(|func| (func.index, func))
        .collect::<HashMap<_, _>>();

    let ctx = CallResolutionContext {
        functions_by_index,
        plain_symbols,
        ambiguous_names,
        signatures,
        extern_names,
        module,
        path,
        graph,
        sources,
        source_id,
    };

    let resolve_stmt = |stmt: &mut Stmt| -> Result<(), SourcePathError> {
        resolve_stmt_imported_calls(&ctx, stmt)
    };
    for stmt in &mut parsed.stmts {
        resolve_stmt(stmt)?;
    }
    for function_impl in parsed.function_impls.values_mut() {
        for stmt in &mut function_impl.body_stmts {
            resolve_stmt(stmt)?;
        }
        resolve_expr_imported_calls(
            &ctx,
            &mut function_impl.body_expr,
            function_impl.body_expr_line.max(1),
        )?;
    }
    Ok(())
}

fn unknown_function_error(
    path: &Path,
    name: &str,
    sources: &SourceMap,
    source_id: u32,
    line: u32,
) -> SourcePathError {
    SourcePathError::Source(SourceError::Parse(ParseError {
        span: sources.line_span(source_id, line as usize),
        code: None,
        line: line as usize,
        message: format!("{}: unknown function '{}'", path.display(), name),
    }))
}

/// Validate type arguments on a host import call whose parse-time validation
/// was deferred (non-builtin namespaces that may name file modules).
fn validate_deferred_host_type_args(
    ctx: &CallResolutionContext<'_>,
    host_name: &str,
    type_args: &[TypeSchema],
    line: u32,
) -> Result<(), SourcePathError> {
    let expected = crate::compiler::parser::host_generic_type_arg_arity(host_name);
    let parse_error = |message: String| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            span: ctx.sources.line_span(ctx.source_id, line as usize),
            code: None,
            line: line as usize,
            message: format!("{}: {message}", ctx.path.display()),
        }))
    };
    match expected {
        Some(expected) if type_args.is_empty() || expected == type_args.len() => Ok(()),
        Some(expected) => Err(parse_error(format!(
            "function '{host_name}' expects {expected} type arguments, got {}",
            type_args.len()
        ))),
        None if type_args.is_empty() => Ok(()),
        None => Err(parse_error(format!(
            "function '{host_name}' does not accept explicit type arguments"
        ))),
    }
}

fn unknown_namespace_call_error(
    path: &Path,
    qualified: &str,
    sources: &SourceMap,
    source_id: u32,
    line: u32,
) -> SourcePathError {
    SourcePathError::Source(SourceError::Parse(ParseError {
        span: sources.line_span(source_id, line as usize),
        code: None,
        line: line as usize,
        message: format!(
            "{}: unknown namespace call '{}'; the module does not export this function",
            path.display(),
            qualified
        ),
    }))
}

fn ambiguous_imported_call_error(
    path: &Path,
    name: &str,
    sources: &SourceMap,
    source_id: u32,
    line: u32,
) -> SourcePathError {
    SourcePathError::Source(SourceError::Parse(ParseError {
        span: sources.line_span(source_id, line as usize),
        code: None,
        line: line as usize,
        message: format!(
            "{}: call to '{name}' is ambiguous: the name is exported by multiple imported modules; qualify the call with a namespace alias or a named import",
            path.display()
        ),
    }))
}

fn resolve_stmt_imported_calls(
    ctx: &CallResolutionContext<'_>,
    stmt: &mut Stmt,
) -> Result<(), SourcePathError> {
    let line = stmt_line(stmt);
    match stmt {
        Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            resolve_expr_imported_calls(ctx, expr, line)?;
        }
        Stmt::ClosureLet { closure, .. } => {
            resolve_expr_imported_calls(ctx, &mut closure.body, line)?;
        }
        Stmt::FuncDecl { .. } => {}
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            resolve_expr_imported_calls(ctx, condition, line)?;
            for nested in then_branch {
                resolve_stmt_imported_calls(ctx, nested)?;
            }
            for nested in else_branch {
                resolve_stmt_imported_calls(ctx, nested)?;
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            resolve_stmt_imported_calls(ctx, init)?;
            resolve_expr_imported_calls(ctx, condition, line)?;
            resolve_stmt_imported_calls(ctx, post)?;
            for nested in body {
                resolve_stmt_imported_calls(ctx, nested)?;
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            resolve_expr_imported_calls(ctx, condition, line)?;
            for nested in body {
                resolve_stmt_imported_calls(ctx, nested)?;
            }
        }
        Stmt::Drop { .. } => {}
    }
    Ok(())
}

fn resolve_expr_imported_calls(
    ctx: &CallResolutionContext<'_>,
    expr: &mut Expr,
    line: u32,
) -> Result<(), SourcePathError> {
    match expr {
        Expr::Call(index, type_args, args, _host_annotation) => {
            for arg in args.iter_mut() {
                resolve_expr_imported_calls(ctx, arg, line)?;
            }
            let Some(decl) = ctx.functions_by_index.get(index) else {
                // Builtin calls use the reserved builtin index space and are
                // not part of the unit's declaration table.
                return Ok(());
            };
            if decl.symbol.is_some() {
                // Local declaration or host import: resolved by the linker.
                // Host imports whose type arguments were deferred at parse
                // (non-builtin namespaces that may name file modules) are
                // validated against the host generic arity here.
                if decl.name.contains("::") {
                    validate_deferred_host_type_args(ctx, &decl.name, type_args, line)?;
                }
                return Ok(());
            }
            let name = decl.name.as_str();
            if let Some(symbol) = ctx.target_for_call(name, args.len(), type_args, line)? {
                // Post-merge annotation ordering invariant: imported-call
                // resolution runs before merge/typing, while the exact host
                // annotation is attached only post-merge, so this loader
                // never receives `Some` here and [`Expr::ModuleCall`] carries
                // no host resolution.
                *expr = Expr::ModuleCall(symbol, std::mem::take(type_args), std::mem::take(args));
            } else {
                return Err(unknown_function_error(
                    ctx.path,
                    name,
                    ctx.sources,
                    ctx.source_id,
                    line,
                ));
            }
        }
        Expr::FunctionRef(index, _type_args) => {
            let Some(decl) = ctx.functions_by_index.get(index) else {
                return Ok(());
            };
            if decl.symbol.is_none() {
                return Err(unknown_function_error(
                    ctx.path,
                    &decl.name,
                    ctx.sources,
                    ctx.source_id,
                    line,
                ));
            }
        }
        Expr::UnresolvedFunctionRef { name, type_args } => {
            if let Some(symbol) = ctx.target_for_function_ref(name, line)? {
                *expr = Expr::ModuleFunctionRef(symbol, std::mem::take(type_args));
            } else {
                return Err(unknown_function_error(
                    ctx.path,
                    name,
                    ctx.sources,
                    ctx.source_id,
                    line,
                ));
            }
        }
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Bytes(_)
        | Expr::String(_)
        | Expr::ModuleCall(..)
        | Expr::ModuleFunctionRef(..)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => {}
        Expr::OptionalGet {
            container,
            key,
            container_slot: _,
            key_slot: _,
        } => {
            resolve_expr_imported_calls(ctx, container, line)?;
            resolve_expr_imported_calls(ctx, key, line)?;
        }
        Expr::OptionUnwrapOr {
            value,
            value_slot: _,
            fallback,
        } => {
            resolve_expr_imported_calls(ctx, value, line)?;
            resolve_expr_imported_calls(ctx, fallback, line)?;
        }
        Expr::LocalCall(_, _, args) => {
            for arg in args.iter_mut() {
                resolve_expr_imported_calls(ctx, arg, line)?;
            }
        }
        Expr::Closure(closure) => {
            resolve_expr_imported_calls(ctx, &mut closure.body, line)?;
        }
        Expr::ClosureCall(closure, args) => {
            resolve_expr_imported_calls(ctx, &mut closure.body, line)?;
            for arg in args.iter_mut() {
                resolve_expr_imported_calls(ctx, arg, line)?;
            }
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
            resolve_expr_imported_calls(ctx, lhs, line)?;
            resolve_expr_imported_calls(ctx, rhs, line)?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            resolve_expr_imported_calls(ctx, inner, line)?;
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            resolve_expr_imported_calls(ctx, condition, line)?;
            resolve_expr_imported_calls(ctx, then_expr, line)?;
            resolve_expr_imported_calls(ctx, else_expr, line)?;
        }
        Expr::Match {
            value_slot: _,
            result_slot: _,
            value,
            arms,
            default,
        } => {
            resolve_expr_imported_calls(ctx, value, line)?;
            for (_, arm_expr) in arms.iter_mut() {
                resolve_expr_imported_calls(ctx, arm_expr, line)?;
            }
            resolve_expr_imported_calls(ctx, default, line)?;
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts.iter_mut() {
                resolve_stmt_imported_calls(ctx, stmt)?;
            }
            resolve_expr_imported_calls(ctx, expr, line)?;
        }
    }
    Ok(())
}

/// Source line of one statement, used to attribute unresolved/ambiguous
/// imported-call diagnostics to the owning module source.
fn stmt_line(stmt: &Stmt) -> u32 {
    match stmt {
        Stmt::Noop { line }
        | Stmt::Break { line }
        | Stmt::Continue { line }
        | Stmt::Drop { line, .. }
        | Stmt::ClosureLet { line, .. }
        | Stmt::FuncDecl { line, .. }
        | Stmt::Let { line, .. }
        | Stmt::Assign { line, .. }
        | Stmt::Expr { line, .. }
        | Stmt::IfElse { line, .. }
        | Stmt::For { line, .. }
        | Stmt::While { line, .. } => *line,
    }
}
