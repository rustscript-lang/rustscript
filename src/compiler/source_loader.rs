//! Semantic file-module loading.
//!
//! This module is the sole file-module path of the compiler (milestone 6):
//! there is no textual import rewriting, no synthetic imported-function
//! prelude, and no prelude line-map remapping anymore. Every module source is
//! parsed verbatim with the real frontend parser; `use` directives become
//! structured [`UseDecl`](crate::compiler::modules::UseDecl) nodes, the
//! [`ModuleGraph`] assigns deterministic [`ModuleId`]s/[`SourceId`]s and
//! records import edges, exports, and imported bindings, and calls to
//! imported functions are resolved to [`SymbolId`]s before unit merge.
//!
//! ## Load pipeline
//!
//! 1. `collect_module_units` discovers the import graph from the root,
//!    registering every module (disk or in-memory override) in the
//!    [`ModuleGraph`] and its raw text in the compilation-wide
//!    [`SourceMap`](crate::compiler::source_map::SourceMap) at its graph
//!    `SourceId`.
//! 2. Each module is parsed in module mode (implicit-extern fallback on):
//!    calls the parser cannot resolve locally — imported module functions,
//!    module namespace members, imported function values — parse into
//!    synthetic externs (tracked on `FrontendIr::implicit_extern_names`) or
//!    `Expr::UnresolvedFunctionRef`, and are resolved afterwards.
//! 3. `record_module_symbols` assigns every real declaration its owned
//!    [`SymbolId`], fills the module's public export table and imported
//!    binding table, then resolves every call site to an `Expr::ModuleCall`
//!    or `Expr::ModuleFunctionRef` carrying the target symbol, validating
//!    arity and type arguments against the exported signature.
//! 4. `linker::merge_units` merges units by symbol identity and applies the
//!    deterministic flat-boundary mangling only there.
//!
//! Host namespace imports (`use io;`, `use myhost;`) never enter the textual
//! machinery: the parser keeps their dedicated host resolution path and the
//! loader records them as host/builtin import edges. Single-segment imports
//! that may name a file module (`use module;`) parse as host-form calls and
//! are fixed up by the loader when the spec resolves to a file module.
//!
//! Every span produced here references the owning module's graph `SourceId`
//! in the compilation-wide map, so diagnostics always render from the
//! owning source.

use std::path::Path;

use crate::compiler::source_map::SourceMap;

use super::modules::ModuleGraph;
use super::{
    CompileSourceFileOptions, SourceError, SourceFlavor, SourcePathError, frontends,
    linker::ParsedUnit,
};

mod graph;
mod imports;
mod model;

use graph::{collect_module_units, record_module_symbols};
use imports::{module_identity, parse_module_imports, strip_import_directives};
use model::ModuleCollectState;
pub use model::{FrontendImportSyntax, ImportClause, ModuleImport, NamedImport};
pub(super) struct LoadedSourceUnits {
    pub(super) units: Vec<ParsedUnit>,
    /// Semantic module graph built during discovery (milestones 1-3).
    pub(super) module_graph: ModuleGraph,
    /// Compilation-wide source map keyed by the module graph's `SourceId`
    /// space (milestone 5). Every module's raw text is registered here at
    /// its graph source id, so spans carried by the loaded units and by any
    /// load-time diagnostic resolve to the owning source.
    pub(super) sources: SourceMap,
}

fn effective_source_options(options: &CompileSourceFileOptions) -> CompileSourceFileOptions {
    options.clone()
}

pub(super) fn load_units_for_source_file(
    path: &Path,
    flavor: SourceFlavor,
    source_raw: &str,
    options: &CompileSourceFileOptions,
) -> Result<LoadedSourceUnits, SourcePathError> {
    let effective_options = effective_source_options(options);
    // The root participates in the same identity scheme as every module:
    // canonical disk identity when the file exists, normalized virtual
    // identity otherwise. This keeps `seen`/`visiting`/exports/overrides
    // keyed uniformly across the whole import graph.
    let path = module_identity(path.to_path_buf());
    let path = path.as_path();

    let mut collect_state = ModuleCollectState::default();
    // Pre-register the root text at its graph source id. The root node is
    // always registered first (SourceId(0)); registering the text here lets
    // the root's own scan/parse diagnostics attach spans against the
    // compilation-wide map before collection runs.
    collect_state
        .sources
        .add_source_at(0, path.display().to_string(), source_raw.to_string());
    collect_state.visiting.push(path.to_path_buf());

    let root_imports = parse_module_imports(source_raw, flavor, path, &effective_options, 0)
        .map_err(|err| {
            // The root's own scan/parse diagnostics attach their span against
            // the pre-registered root source and carry the compilation-wide map,
            // so they render from the root's text.
            match err {
                SourcePathError::Source(SourceError::Parse(mut parse)) => {
                    parse.span = None;
                    parse = parse.with_line_span_from_source(&collect_state.sources, 0);
                    SourcePathError::SourceWithMap {
                        error: SourceError::Parse(parse),
                        sources: collect_state.sources.clone(),
                    }
                }
                other => other,
            }
        })?;

    collect_module_units(
        path,
        source_raw,
        flavor,
        &effective_options,
        &mut collect_state,
    )
    .map_err(|err| {
        // Load-time source diagnostics (nested scan/parse errors, symbol
        // resolution, imported-call resolution) already carry spans keyed to
        // the compilation-wide map; attach the map so they render from the
        // owning source.
        match err {
            SourcePathError::Source(error) => SourcePathError::SourceWithMap {
                error,
                sources: collect_state.sources.clone(),
            },
            other => other,
        }
    })?;
    let root_module = collect_state
        .module_graph
        .module_id_for_identity(path)
        .expect("root module should be registered in the module graph");
    let root_source_id = collect_state
        .module_graph
        .node(root_module)
        .map(|node| node.source.0)
        .unwrap_or(0);
    let root_parse_source = strip_import_directives(source_raw, flavor, &effective_options)?;

    let mut root_parsed = frontends::parse_module_source_with_source_id(
        &root_parse_source,
        flavor,
        &effective_options,
        root_source_id,
    )
    .map_err(|mut err| {
        // Module sources are parsed verbatim (no synthetic prelude, no
        // textual rewrite), so parse lines already refer to the owning
        // source; rebuild the span against the compilation-wide map so the
        // diagnostic renders from the root's text.
        err.span = None;
        let parse = err.with_line_span_from_source(&collect_state.sources, root_source_id);
        SourcePathError::SourceWithMap {
            error: SourceError::Parse(parse),
            sources: collect_state.sources.clone(),
        }
    })?;
    record_module_symbols(
        &mut collect_state,
        root_module,
        path,
        &root_imports,
        &mut root_parsed,
        &effective_options,
    )
    .map_err(|err| match err {
        // Root resolution diagnostics (unknown/ambiguous imported calls,
        // visibility failures) already carry spans keyed to the
        // compilation-wide map; attach the map so they render from the
        // owning source.
        SourcePathError::Source(error) => SourcePathError::SourceWithMap {
            error,
            sources: collect_state.sources.clone(),
        },
        other => other,
    })?;
    collect_state.units.push(ParsedUnit {
        parsed: root_parsed,
        scope_identity: None,
        source_name: path.display().to_string(),
        module: root_module,
        source_id: root_source_id,
        host_catalog_supplied: effective_options.host_api_catalog().is_some(),
    });

    Ok(LoadedSourceUnits {
        units: collect_state.units,
        module_graph: collect_state.module_graph,
        sources: collect_state.sources,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::modules::ModuleId;
    use super::*;

    fn temp_module_root(prefix: &str) -> PathBuf {
        let unique = format!(
            "{prefix}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).expect("temp module root should be created");
        // Module identities are canonical for existing files; keep expected
        // paths canonical too so assertions match under symlinked temp dirs.
        root.canonicalize().unwrap_or(root)
    }

    fn write_source(path: &Path, source: &str, description: &str) {
        std::fs::write(path, source)
            .unwrap_or_else(|err| panic!("{description} should write: {err}"));
    }

    fn remove_module_root(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    /// Two modules named `util.rss` in different directories plus a root that
    /// imports both. Returns `(main, a/util, b/util)` paths.
    fn write_same_stem_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let a_dir = root.join("a");
        let b_dir = root.join("b");
        std::fs::create_dir_all(&a_dir).expect("a dir should be created");
        std::fs::create_dir_all(&b_dir).expect("b dir should be created");

        let a_module = a_dir.join("util.rss");
        let b_module = b_dir.join("util.rss");
        write_source(&a_module, "pub fn helper() { 1; }\n", "a/util source");
        write_source(&b_module, "pub fn helper() { 2; }\n", "b/util source");

        let main_path = root.join("main.rss");
        write_source(
            &main_path,
            "use a::util as au;\nuse b::util as bu;\nfn run() { au::helper(); bu::helper(); }\n",
            "main source",
        );
        (main_path, a_module, b_module)
    }

    #[test]
    fn loader_graph_records_same_stem_modules_in_different_directories() {
        let root = temp_module_root("semantic_m1_same_stem");
        let (main_path, a_module, b_module) = write_same_stem_fixture(&root);
        let main_source = std::fs::read_to_string(&main_path).expect("main source readable");

        let loaded = load_units_for_source_file(
            &main_path,
            SourceFlavor::RustScript,
            &main_source,
            &CompileSourceFileOptions::default(),
        )
        .expect("load should succeed");
        let graph = loaded.module_graph;
        assert_eq!(graph.len(), 3, "root plus two same-stem modules");

        let main_id = graph
            .module_id_for_identity(&main_path)
            .expect("main module should be registered");
        let a_id = graph
            .module_id_for_identity(&a_module)
            .expect("a/util should be registered");
        let b_id = graph
            .module_id_for_identity(&b_module)
            .expect("b/util should be registered");
        assert_ne!(
            a_id, b_id,
            "same-stem modules in different dirs must differ"
        );
        assert_eq!(main_id, ModuleId(0), "root module is always module 0");

        // Import edges from the root to both modules, in source order.
        let main_node = graph.node(main_id).expect("main node should exist");
        assert_eq!(main_node.imports.len(), 2);
        let targets: Vec<_> = main_node
            .imports
            .iter()
            .map(|import| import.target)
            .collect();
        assert!(targets.contains(&Some(a_id)));
        assert!(targets.contains(&Some(b_id)));
        assert!(main_node.imports.iter().all(|import| import.line >= 1));
        assert_eq!(main_node.imports[0].spec, "a/util.rss");
        assert_eq!(main_node.imports[1].spec, "b/util.rss");

        remove_module_root(&root);
    }

    #[test]
    fn loader_graph_is_deterministic_across_loads() {
        let root = temp_module_root("semantic_m1_deterministic");
        let (main_path, _, _) = write_same_stem_fixture(&root);

        let load = || {
            let source = std::fs::read_to_string(&main_path).expect("main source readable");
            load_units_for_source_file(
                &main_path,
                SourceFlavor::RustScript,
                &source,
                &CompileSourceFileOptions::default(),
            )
            .expect("load should succeed")
            .module_graph
        };
        let first = load();
        let second = load();
        assert_eq!(first.len(), second.len());
        let sequence = |graph: &ModuleGraph| {
            graph
                .nodes()
                .iter()
                .map(|node| (node.module, node.identity.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(sequence(&first), sequence(&second));

        remove_module_root(&root);
    }

    #[test]
    fn loader_graph_uses_virtual_identity_for_in_memory_modules() {
        let path = PathBuf::from("__pd_vm_inmemory__/main.rss");
        let source = "use a::util;\nfn run() { helper(); }\n";
        let options = CompileSourceFileOptions::new()
            .with_module_override_source("a/util.rss", "pub fn helper() { 1; }\n");

        let loaded = load_units_for_source_file(&path, SourceFlavor::RustScript, source, &options)
            .expect("virtual load should succeed");
        let graph = loaded.module_graph;
        assert_eq!(graph.len(), 2, "virtual root plus overridden module");

        let main_id = graph
            .module_id_for_identity(&path)
            .expect("virtual main should be registered");
        let a_id = graph
            .module_id_for_identity(PathBuf::from("__pd_vm_inmemory__/a/util.rss").as_path())
            .expect("virtual override module should be registered");
        assert_ne!(main_id, a_id);

        let main_node = graph.node(main_id).expect("main node should exist");
        assert_eq!(main_node.imports.len(), 1);
        assert_eq!(main_node.imports[0].target, Some(a_id));
        assert_eq!(
            main_node.imports[0].kind,
            super::super::modules::ImportTargetKind::FileModule
        );
    }

    /// Fixture: `main` imports `a/util` (pub alpha + private helper) and
    /// `b/util` (pub beta + private helper). Both helpers are private and
    /// same-named; `a/util` also imports a third module `leaf` (pub shared)
    /// so the transitive re-export rule is exercised through the real loader.
    fn write_symbol_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let a_dir = root.join("a");
        let b_dir = root.join("b");
        std::fs::create_dir_all(&a_dir).expect("a dir should be created");
        std::fs::create_dir_all(&b_dir).expect("b dir should be created");

        let leaf_module = a_dir.join("leaf.rss");
        write_source(&leaf_module, "pub fn shared() { 100; }\n", "leaf source");

        let a_module = a_dir.join("util.rss");
        write_source(
            &a_module,
            "use self::leaf;\npub fn alpha() { helper(); }\nfn helper() { 11; }\n",
            "a/util source",
        );
        let b_module = b_dir.join("util.rss");
        write_source(
            &b_module,
            "pub fn beta() { helper(); }\nfn helper() { 22; }\n",
            "b/util source",
        );

        let main_path = root.join("main.rss");
        write_source(
            &main_path,
            "use a::util as au;\nuse b::util as bu;\nfn run() { au::alpha(); bu::beta(); }\n",
            "main source",
        );
        (main_path, a_module, b_module, leaf_module)
    }

    fn load_fixture(main_path: &Path) -> ModuleGraph {
        let main_source = std::fs::read_to_string(main_path).expect("main source readable");
        load_units_for_source_file(
            main_path,
            SourceFlavor::RustScript,
            &main_source,
            &CompileSourceFileOptions::default(),
        )
        .expect("load should succeed")
        .module_graph
    }

    #[test]
    fn loader_records_public_exports_and_private_declarations() {
        let root = temp_module_root("semantic_m3_exports");
        let (main_path, a_module, b_module, _) = write_symbol_fixture(&root);
        let graph = load_fixture(&main_path);

        let a_id = graph
            .module_id_for_identity(&a_module)
            .expect("a/util should be registered");
        let a_node = graph.node(a_id).expect("a/util node exists");
        let a_names = |node: &super::super::modules::ModuleNode| {
            node.declarations
                .iter()
                .map(|decl| decl.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(a_names(a_node), vec!["alpha", "helper"]);
        assert_eq!(
            a_node
                .exports
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha"],
            "only the pub declaration is exported"
        );
        assert!(
            !graph
                .declaration(a_id, "helper")
                .expect("private helper exists")
                .public
        );
        assert_eq!(
            graph.symbol_for_export(a_id, "helper"),
            None,
            "private helpers never enter the export table"
        );

        let b_id = graph
            .module_id_for_identity(&b_module)
            .expect("b/util should be registered");
        assert_eq!(
            graph
                .symbol_for_export(b_id, "beta")
                .expect("beta is exported")
                .module,
            b_id,
            "each module's exports are owned by that module"
        );

        remove_module_root(&root);
    }

    #[test]
    fn loader_keeps_imported_bindings_separate_and_blocks_transitive_reexport() {
        let root = temp_module_root("semantic_m3_bindings");
        let (main_path, a_module, _, leaf_module) = write_symbol_fixture(&root);
        let graph = load_fixture(&main_path);

        let a_id = graph
            .module_id_for_identity(&a_module)
            .expect("a/util should be registered");
        let a_node = graph.node(a_id).expect("a/util node exists");
        assert_eq!(
            a_node.imported_bindings.len(),
            1,
            "a/util imports exactly leaf::shared"
        );
        let binding = &a_node.imported_bindings[0];
        assert_eq!(binding.local_name, "shared");
        assert_eq!(binding.source_name, "shared");
        assert_eq!(
            binding.source_module,
            graph
                .module_id_for_identity(&leaf_module)
                .expect("leaf should be registered")
        );
        assert_eq!(
            graph
                .symbol_for_export(a_id, "shared")
                .map(|symbol| symbol.module),
            None,
            "a/util must not re-export leaf's function"
        );
        assert!(
            a_node.declarations.iter().all(|decl| decl.name != "shared"),
            "the imported function is not a local declaration of a/util"
        );

        let main_id = graph
            .module_id_for_identity(&main_path)
            .expect("main should be registered");
        let main_node = graph.node(main_id).expect("main node exists");
        assert_eq!(
            main_node.imported_bindings.len(),
            2,
            "main imports alpha and beta"
        );
        assert!(
            main_node
                .imported_bindings
                .iter()
                .all(|binding| binding.local_name == "alpha" || binding.local_name == "beta")
        );
        assert!(
            main_node.imported_bindings.iter().all(|binding| {
                graph
                    .symbol_for_export(main_id, &binding.local_name)
                    .is_none()
            }),
            "main's export table stays empty: no implicit re-export of anything imported"
        );

        remove_module_root(&root);
    }

    #[test]
    fn loader_assigns_distinct_symbols_to_same_named_private_helpers() {
        let root = temp_module_root("semantic_m3_same_named");
        let (main_path, a_module, b_module, _) = write_symbol_fixture(&root);
        let graph = load_fixture(&main_path);

        let a_id = graph
            .module_id_for_identity(&a_module)
            .expect("a/util should be registered");
        let b_id = graph
            .module_id_for_identity(&b_module)
            .expect("b/util should be registered");
        let a_helper = graph
            .declaration_symbol(a_id, "helper")
            .expect("a helper exists");
        let b_helper = graph
            .declaration_symbol(b_id, "helper")
            .expect("b helper exists");
        assert_ne!(
            a_helper, b_helper,
            "same-named private helpers in independent modules own distinct symbols"
        );
        assert_eq!(a_helper.module, a_id);
        assert_eq!(b_helper.module, b_id);

        let main_id = graph
            .module_id_for_identity(&main_path)
            .expect("main should be registered");
        let main_alpha = graph
            .declaration_symbol(main_id, "run")
            .expect("run exists");
        assert_eq!(
            main_alpha,
            super::super::modules::SymbolId {
                module: main_id,
                index: 0
            },
            "root declarations start at symbol index 0"
        );

        remove_module_root(&root);
    }

    #[test]
    fn loader_symbols_are_deterministic_across_loads() {
        let root = temp_module_root("semantic_m3_symbol_determinism");
        let (main_path, a_module, _, _) = write_symbol_fixture(&root);
        let first = load_fixture(&main_path);
        let second = load_fixture(&main_path);

        let symbol_sequence = |graph: &ModuleGraph| {
            graph
                .nodes()
                .iter()
                .map(|node| {
                    (
                        node.module,
                        node.declarations
                            .iter()
                            .map(|decl| (decl.name.clone(), decl.symbol))
                            .collect::<Vec<_>>(),
                        node.exports
                            .iter()
                            .map(|entry| (entry.name.clone(), entry.symbol))
                            .collect::<Vec<_>>(),
                        node.imported_bindings
                            .iter()
                            .map(|binding| {
                                (
                                    binding.local_name.clone(),
                                    binding.source_symbol,
                                    binding.source_module,
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(symbol_sequence(&first), symbol_sequence(&second));
        assert_eq!(first.len(), second.len());
        let _ = a_module;

        remove_module_root(&root);
    }

    /// The parser-assigned semantic id of a module namespace / imported call
    /// survives the source-loader `Expr::Call -> Expr::ModuleCall` rewrite
    /// and the linker's `Expr::ModuleCall -> Expr::Call` lowering, and the
    /// parsed call-site target is upgraded to the resolved module symbol
    /// along the way. When the linker merges several units, every id is
    /// rebased onto a collision-free merged id space; the invariant is that
    /// the final flat `Call` node and the merged parsed index record the
    /// *same* (rebased) id for the same source call.
    #[test]
    fn module_call_semantic_id_survives_loader_and_linker() {
        use super::super::ir::{Expr, ParsedCallTarget};
        use super::super::linker::merge_units;

        let path = PathBuf::from("__pd_vm_inmemory__/main.rss");
        let source = "use a::util as au;\nfn run() { au::helper(); }\n";
        let options = CompileSourceFileOptions::new()
            .with_module_override_source("a/util.rss", "pub fn helper() { 7; }\n");

        let loaded = load_units_for_source_file(&path, SourceFlavor::RustScript, source, &options)
            .expect("virtual load should succeed");
        assert_eq!(loaded.units.len(), 2, "root plus overridden module");

        let root_unit = loaded
            .units
            .iter()
            .find(|unit| unit.source_name.ends_with("main.rss"))
            .expect("root unit present");
        let parsed = root_unit
            .parsed
            .parsed_semantic_index
            .as_ref()
            .expect("root parse carries provenance");
        assert_eq!(
            parsed.call_sites.len(),
            1,
            "one namespace call recorded by the parser"
        );

        // The loader must have rewritten the call to a ModuleCall carrying
        // the same id the parser assigned.
        let module_call = loaded
            .units
            .iter()
            .flat_map(|unit| unit.parsed.function_impls.values())
            .filter_map(|impl_| match &impl_.body_expr {
                Expr::ModuleCall(symbol, _, _, semantic_id) => Some((*symbol, *semantic_id)),
                _ => None,
            })
            .next()
            .expect("loader rewrote the namespace call to a ModuleCall");
        let (symbol, loader_id) = module_call;
        let Some(loader_id) = loader_id else {
            panic!("ModuleCall must carry the parser semantic id");
        };

        // The parsed call site records the same id and an upgraded module
        // target matching the ModuleCall's symbol.
        let site = parsed
            .call_sites
            .iter()
            .find(|site| site.id == loader_id)
            .expect("call site matches the ModuleCall id");
        match site.target {
            ParsedCallTarget::Module(site_symbol) => {
                assert_eq!(site_symbol, symbol, "site target is the resolved symbol")
            }
            ref other => panic!("expected Module target after loader, got {other:?}"),
        }
        // N1: the callee span is the exact namespace path token range
        // (`au::helper`), never the whole call including arguments, and the
        // expr span covers the full call through the closing `)`.
        let callee_slice = &source[site.callee_span.lo..site.callee_span.hi];
        let expr_slice = &source[site.expr_span.lo..site.expr_span.hi];
        assert_eq!(
            callee_slice, "au::helper",
            "exact namespace path callee slice"
        );
        assert_eq!(expr_slice, "au::helper()", "exact full call slice");
        assert!(
            site.expr_span.hi > site.callee_span.hi,
            "expr span extends past the callee over the argument list"
        );
        // N4: the parser-recorded function-value reference for the
        // implicit-extern callee must be upgraded to the resolved module
        // symbol, never left with a stale unit-local flat index. (Namespace
        // calls record no function-value ref — only direct imported calls
        // do, covered by the dedicated test below.)
        assert!(
            parsed
                .func_refs
                .iter()
                .all(|reference| reference.name != "au::helper"),
            "namespace call records no func_ref"
        );
        assert_eq!(
            site.callee_span.lo, site.expr_span.lo,
            "expr span starts at the callee start"
        );

        // The linker lowers ModuleCall -> Call and rebases the id onto the
        // merged collision-free space. The invariant is consistency: the
        // final flat Call id equals the merged parsed index's call-site id
        // for the same source call (the unit-local parser id may be rebased
        // when an earlier-merged unit consumed leading node ids).
        let merged = merge_units(loaded.units).expect("merge must succeed");
        let final_call = merged
            .function_impls
            .values()
            .filter_map(|impl_| match &impl_.body_expr {
                Expr::Call(_, _, _, _, semantic_id) => Some(*semantic_id),
                _ => None,
            })
            .next()
            .expect("merged IR lowers the call to a flat Call")
            .expect("final flat Call carries a semantic id");
        let merged_index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        let merged_site = merged_index
            .call_sites
            .iter()
            .find(|site| site.name == "au::helper")
            .expect("merged index records the namespace call");
        assert_eq!(
            final_call, merged_site.id,
            "final flat Call id matches the merged index entry"
        );

        remove_module_root(std::path::Path::new("__pd_vm_inmemory__"));
    }

    /// Loader-resolved module function-value references (`let f = helper;`
    /// in module mode) must not leave stale unit-local flat indices in the
    /// merged carrier. The parser records a placeholder flat target; the
    /// loader upgrades the matching `func_ref` to `Module(symbol)` and the
    /// linker preserves that module target verbatim through the merge.
    #[test]
    fn module_function_value_refs_upgrade_to_symbol_and_survive_merge() {
        use super::super::ir::{Expr, FunctionRefTarget};
        use super::super::linker::merge_units;

        let path = PathBuf::from("__pd_vm_inmemory__/main.rss");
        let source = "use a::util::{helper};\nfn run() { let f = helper; f; }\n";
        let options = CompileSourceFileOptions::new()
            .with_module_override_source("a/util.rss", "pub fn helper() { 7; }\n");

        let loaded = load_units_for_source_file(&path, SourceFlavor::RustScript, source, &options)
            .expect("virtual load should succeed");
        assert_eq!(loaded.units.len(), 2, "root plus overridden module");

        let root_unit = loaded
            .units
            .iter()
            .find(|unit| unit.source_name.ends_with("main.rss"))
            .expect("root unit present");
        let parsed = root_unit
            .parsed
            .parsed_semantic_index
            .as_ref()
            .expect("root parse carries provenance");

        // The function-value reference's placeholder flat target must have
        // been upgraded to the resolved module symbol by the loader.
        let reference = parsed
            .func_refs
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("helper function value ref recorded");
        let symbol = match reference.target {
            FunctionRefTarget::Module(symbol) => symbol,
            ref other => panic!("expected Module target after loader, got {other:?}"),
        };

        // The loader also rewrote the Expr to a ModuleFunctionRef carrying
        // the same symbol.
        let module_ref = root_unit
            .parsed
            .function_impls
            .values()
            .find_map(|impl_| match &impl_.body_expr {
                Expr::ModuleFunctionRef(s, _) => Some(*s),
                _ => impl_.body_stmts.iter().find_map(|stmt| match stmt {
                    crate::compiler::ir::Stmt::Let {
                        expr: Expr::ModuleFunctionRef(s, _),
                        ..
                    } => Some(*s),
                    _ => None,
                }),
            })
            .expect("loader rewrote the function value ref to ModuleFunctionRef");
        assert_eq!(module_ref, symbol, "Expr and func_ref share the symbol");

        // After the merge, the func_ref keeps its Module target (no flat
        // index rebase applies) and the lowered FunctionRef carries the
        // merged flat index.
        let merged = merge_units(loaded.units).expect("merge must succeed");
        let merged_index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        let merged_ref = merged_index
            .func_refs
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("merged func_ref present");
        assert_eq!(
            merged_ref.target,
            FunctionRefTarget::Module(symbol),
            "module target survives merge verbatim"
        );

        remove_module_root(std::path::Path::new("__pd_vm_inmemory__"));
    }

    /// A direct imported call (`helper()` where `helper` is a named import)
    /// records a function-value reference via `attach_ordinary_call_provenance`
    /// with a unit-local flat index; the loader must upgrade that reference
    /// to `Module(symbol)` so the merged carrier never aliases an unrelated
    /// flat function.
    #[test]
    fn direct_imported_call_func_ref_upgrades_to_symbol() {
        use super::super::ir::{Expr, FunctionRefTarget};
        use super::super::linker::merge_units;

        let path = PathBuf::from("__pd_vm_inmemory__/main.rss");
        let source = "use a::util::{helper};\nfn run() { helper(); }\n";
        let options = CompileSourceFileOptions::new()
            .with_module_override_source("a/util.rss", "pub fn helper() { 7; }\n");

        let loaded = load_units_for_source_file(&path, SourceFlavor::RustScript, source, &options)
            .expect("virtual load should succeed");
        assert_eq!(loaded.units.len(), 2, "root plus overridden module");

        let root_unit = loaded
            .units
            .iter()
            .find(|unit| unit.source_name.ends_with("main.rss"))
            .expect("root unit present");
        let parsed = root_unit
            .parsed
            .parsed_semantic_index
            .as_ref()
            .expect("root parse carries provenance");

        // The direct call records one func_ref for the implicit-extern
        // callee; the loader must have upgraded it to the module symbol.
        let helper_refs = parsed
            .func_refs
            .iter()
            .filter(|reference| reference.name == "helper")
            .collect::<Vec<_>>();
        assert_eq!(
            helper_refs.len(),
            1,
            "direct imported call records one callee func ref"
        );
        let symbol = match helper_refs[0].target {
            FunctionRefTarget::Module(symbol) => symbol,
            ref other => panic!("expected Module target after loader, got {other:?}"),
        };

        // The call itself was rewritten to ModuleCall with the same symbol.
        let module_call = root_unit
            .parsed
            .function_impls
            .values()
            .find_map(|impl_| match &impl_.body_expr {
                Expr::ModuleCall(s, _, _, _) => Some(*s),
                _ => None,
            })
            .expect("loader rewrote the call to ModuleCall");
        assert_eq!(module_call, symbol, "call and func ref share the symbol");

        // The merged carrier keeps the Module target (never a stale flat
        // index in the merged function space).
        let merged = merge_units(loaded.units).expect("merge must succeed");
        let merged_index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        let merged_ref = merged_index
            .func_refs
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("merged func_ref present");
        assert_eq!(
            merged_ref.target,
            FunctionRefTarget::Module(symbol),
            "module target survives merge verbatim"
        );

        remove_module_root(std::path::Path::new("__pd_vm_inmemory__"));
    }
}
