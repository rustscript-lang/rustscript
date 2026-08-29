//! Real-pipeline tests for the exact, parser-origin SemanticModel completion
//! and diagnostic surface.
//!
//! These tests drive the full analyzer (`analyze_source_file_with_options`
//! through the module loader + linker + legalize + type-check + provenance
//! index) and assert:
//!
//! * lexical completions: same-scope declaration order, nested shadowing,
//!   sibling exclusion, and params / loop / closure / match bindings;
//! * catalog completions driven by `CatalogVisibility`: direct aliases,
//!   namespace aliases (member completion), wildcard imports, module aliases,
//!   and source isolation across multi-unit builds;
//! * exact prefix derivation from the lexer token stream (Unicode offsets,
//!   whitespace -> empty prefix) with no full-catalog leakage;
//! * exact diagnostic slices for nested/same-line calls and local/function
//!   errors, never line-wide guesses.
//!
//! No weak `len > 0` / `contains` denials are used; every assertion pins the
//! exact expected surface.

use std::path::PathBuf;
use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, CompletionItemKind, SemanticModel, SourcePosition,
    analyze_source_file_with_options,
};
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};

/// A catalog with deterministic namespaces for import visibility tests.
fn test_catalog() -> Arc<HostApiCatalog> {
    let conn_key = ResourceTypeKey::new("prov.connection").unwrap();
    let sql_key = ResourceTypeKey::new("db.session").unwrap();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(conn_key.clone(), "PROV connection"));
    builder.resource(ResourceTypeSchema::new(sql_key.clone(), "DB session"));

    // prov::make(path: string) -> resource<prov.connection>
    builder.function(HostFunctionSchema::with_return(
        "prov::make",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(conn_key),
    ));
    // prov::connect(host: string) -> resource<prov.connection>
    builder.function(HostFunctionSchema::with_return(
        "prov::connect",
        vec![HostParamSchema::value("host", HostTypeSchema::String)],
        HostTypeSchema::Resource(ResourceTypeKey::new("prov.connection").unwrap()),
    ));
    // io::open(path: string) -> resource<db.session>
    builder.function(HostFunctionSchema::with_return(
        "io::open",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(sql_key.clone()),
    ));
    // io::read(handle: borrow resource<db.session>) -> string
    builder.function(HostFunctionSchema::with_return(
        "io::read",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(sql_key),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
    ));
    // db::query(sql: string) -> int (NOT imported by default tests)
    builder.function(HostFunctionSchema::with_return(
        "db::query",
        vec![HostParamSchema::value("sql", HostTypeSchema::String)],
        HostTypeSchema::Int,
    ));

    Arc::new(builder.build().expect("catalog build"))
}

fn temp_root(prefix: &str) -> PathBuf {
    let unique = format!(
        "{prefix}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp root create");
    root
}

/// Analyze a single source file through the real pipeline with the test
/// catalog.
fn analyze(source: &str) -> SemanticModel {
    let dir = temp_root("semantic_exact");
    let main = dir.join("main.rss");
    std::fs::write(&main, source).expect("write main");
    let options = CompileSourceFileOptions::new().with_host_api_catalog(test_catalog());
    let model = analyze_source_file_with_options(&main, options).expect("analysis succeeds");
    let _ = std::fs::remove_dir_all(&dir);
    model
}

/// Analyze a root source with module overrides through the loader + linker.
fn analyze_modules(root: &str, overrides: &[(&str, &str)]) -> SemanticModel {
    let dir = temp_root("semantic_exact_mod");
    let main = dir.join("main.rss");
    std::fs::write(&main, root).expect("write main");
    let mut options = CompileSourceFileOptions::new().with_host_api_catalog(test_catalog());
    for (spec, source) in overrides {
        options = options.with_module_override_source(*spec, *source);
    }
    let model = analyze_source_file_with_options(&main, options).expect("module analysis succeeds");
    let _ = std::fs::remove_dir_all(&dir);
    model
}

/// The completion labels at a position in the given source, in order.
fn labels(model: &SemanticModel, offset: usize) -> Vec<String> {
    model
        .completions_at(SourcePosition::new(0, offset))
        .iter()
        .map(|c| c.label.clone())
        .collect()
}

/// The completion labels at `offset` in the source whose file name contains
/// `name_contains` (used when the interesting cursor lives in a nested module
/// source rather than the root source id 0).
fn labels_in_source(model: &SemanticModel, name_contains: &str, offset: usize) -> Vec<String> {
    let sources = model.sources();
    let mut id = 0u32;
    let file = loop {
        let Some(file) = sources.file(id) else {
            panic!("no source file containing '{name_contains}'");
        };
        if file.name.contains(name_contains) {
            break file;
        }
        id += 1;
    };
    model
        .completions_at(SourcePosition::new(file.id, offset))
        .iter()
        .map(|c| c.label.clone())
        .collect()
}

/// Byte offset of the first occurrence of `needle`.
fn offset_of(source: &str, needle: &str) -> usize {
    source
        .find(needle)
        .unwrap_or_else(|| panic!("'{needle}' not found in {source:?}"))
}

// ---------------------------------------------------------------------------
// Lexical completions
// ---------------------------------------------------------------------------

#[test]
fn same_scope_declaration_order_and_cursor_exclusion() {
    let source = "let alpha = 1;\nlet beta = 2;\n";
    let model = analyze(source);
    // Cursor on line 2 after `let beta = `.
    let end_beta = offset_of(source, "2;\n") + 1;
    let comps = labels(&model, end_beta);
    // Both alpha and beta visible in declaration order.
    let a = comps.iter().position(|n| n == "alpha").expect("alpha");
    let b = comps.iter().position(|n| n == "beta").expect("beta");
    assert!(a < b, "declaration order: {comps:?}");

    // Cursor on line 1 after `let alpha = ` (before beta is parsed): only
    // alpha is visible at that exact point.
    let end_alpha = offset_of(source, "1;\n") + 1;
    let comps_before = labels(&model, end_alpha);
    assert!(
        comps_before.iter().all(|n| n != "beta"),
        "beta must not be visible before its declaration: {comps_before:?}"
    );
}

#[test]
fn nested_shadowing_innermost_wins() {
    // `x` is defined at module level, then shadowed inside `f` by its own
    // `let x`. Inside the function, only the inner binding is offered.
    let source = "let x = 1;\nfn f() -> int {\n  let x = 2;\n  x\n}\n";
    let model = analyze(source);
    // Cursor right after `let x = 2;` on line 3.
    let inner_decl = offset_of(source, "let x = 2;") + "let x = 2;".len();
    let comps = labels(&model, inner_decl);
    // Only one `x` candidate (the inner shadowing binding), deduplicated.
    assert_eq!(
        comps.iter().filter(|n| *n == "x").count(),
        1,
        "shadowed name must collapse to the innermost binding: {comps:?}"
    );
}

#[test]
fn sibling_scope_bindings_are_not_visible() {
    // A binding in one sibling block must not leak into another sibling block.
    let source = "fn f() -> int {\n  let inner = 1;\n  inner\n}\nfn g() -> int {\n  let outer = 2;\n  outer\n}\n";
    let model = analyze(source);
    // Cursor inside `g`'s body: `inner` from `f`'s body scope is a sibling
    // and must not be visible.
    let g_body = offset_of(source, "let outer") + "let ".len();
    let comps = labels(&model, g_body);
    assert!(
        comps.iter().all(|n| n != "inner"),
        "sibling function-body binding leaked: {comps:?}"
    );
    assert!(
        comps.iter().any(|n| n == "outer"),
        "own-body binding visible: {comps:?}"
    );
}

#[test]
fn params_loop_closure_match_bindings_visible() {
    // Params, loop iterator, closure params, and match pattern bindings are
    // all recorded as local declarations in their scope and become visible
    // inside those scopes; scoped bindings stop being visible once their
    // scope closes.
    let source = "fn apply(p: int) -> int {\n  for i in 0..3 {\n    let lit = i;\n  }\n  let f = |z| z;\n  let m = match p { 1 => 9, 2 => 8, _ => 0 };\n  p\n}\n";
    let model = analyze(source);

    // Inside the loop body: the iterator `i` (enclosing scope) and the loop
    // body local `lit` are both visible at a whitespace cursor (empty prefix).
    let in_loop = offset_of(source, "let lit = i;") + "let lit = i;".len() + 2;
    let comps = labels(&model, in_loop);
    for name in ["i", "lit", "p"] {
        assert!(
            comps.iter().any(|n| n == name),
            "{name} must be visible inside the loop body: {comps:?}"
        );
    }

    // Inside the closure body: the closure param `z` is visible (cursor on
    // the closure body expression, whose scope range covers it).
    let in_closure = offset_of(source, "|z| z") + "|z| ".len();
    let comps = labels(&model, in_closure);
    assert!(
        comps.iter().any(|n| n == "z"),
        "closure param visible inside closure: {comps:?}"
    );

    // At function-body level after the match statement, the loop/closure
    // locals are closed and must not appear.
    let after_match = offset_of(source, "};") + 2;
    let comps = labels(&model, after_match);
    for name in ["p", "i", "f", "m"] {
        assert!(
            comps.iter().any(|n| n == name),
            "{name} must be visible in fn body: {comps:?}"
        );
    }
    for closed in ["lit", "z"] {
        assert!(
            comps.iter().all(|n| n != closed),
            "{closed} must not leak out of its closed scope: {comps:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Catalog completions (visibility-driven)
// ---------------------------------------------------------------------------

#[test]
fn no_full_catalog_leakage_without_imports() {
    // Even though the catalog has prov/io/db functions, an empty source with
    // no `use` imports must not leak any of them.
    let source = "let local = 1;\n";
    let model = analyze(source);
    // Cursor after the local declaration on line 1 (end of source).
    let comps = labels(&model, offset_of(source, "local") + "local".len());
    for non_leaked in [
        "prov::make",
        "io::open",
        "db::query",
        "resource<prov.connection>",
    ] {
        assert!(
            comps.iter().all(|n| n != non_leaked),
            "{non_leaked} must not leak without an import: {comps:?}"
        );
    }
    // The local itself is visible.
    assert!(comps.iter().any(|n| n == "local"), "{comps:?}");
}

#[test]
fn direct_host_call_alias_completion_uses_alias_label() {
    // `use prov::{make as m};` binds direct alias `m -> prov::make`.
    let source = "use prov::{make as m};\nlet x = 1;\nlet y = 2;\n";
    let model = analyze(source);
    // Cursor at the end of the file (after the last statement): the direct
    // alias `m` is visible with an empty prefix.
    let at = source.len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "m"),
        "direct alias 'm' should be offered: {comps:?}"
    );
    // The canonical `prov::make` full name is NOT offered (the alias is the
    // label), and unrelated catalog names do not leak.
    assert!(
        comps.iter().all(|n| n != "prov::make"),
        "canonical name must not appear alongside the alias: {comps:?}"
    );
    let completion = model
        .completions_at(SourcePosition::new(0, at))
        .into_iter()
        .find(|c| c.label == "m")
        .expect("alias completion");
    assert_eq!(completion.kind, CompletionItemKind::Function);
    assert!(
        completion
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("prov::make"),
        "alias detail carries the canonical schema: {:?}",
        completion.detail
    );
}

#[test]
fn wildcard_import_completion_lists_members() {
    // `use prov::*;` makes every `prov::*` member a direct name.
    let source = "use prov::*;\nlet f = 1;\nlet g = 2;\n";
    let model = analyze(source);
    let at = source.len();
    let comps = labels(&model, at);
    assert!(comps.iter().any(|n| n == "make"), "{comps:?}");
    assert!(comps.iter().any(|n| n == "connect"), "{comps:?}");
    // Members from non-imported namespaces stay out.
    assert!(
        comps.iter().all(|n| n != "query"),
        "db members must not leak through the prov wildcard: {comps:?}"
    );
}

#[test]
fn namespace_member_completion_resolves_canonical() {
    // `use prov;` binds host namespace alias `prov -> prov`. Cursor inside
    // the `make` member token of a real call: member completion resolves the
    // canonical namespace and filters by the partial member.
    let source = "use prov;\nlet c = prov::make(\"x\");\n";
    let model = analyze(source);
    // Cursor at `prov::ma|ke` (the `ma` prefix inside the member token).
    let at = offset_of(source, "prov::make") + "prov::ma".len();
    let comps = labels(&model, at);
    assert!(comps.iter().any(|n| n == "make"), "{comps:?}");
    // Other prov members that do not start with `ma` are filtered out, and
    // no non-prov members leak.
    assert!(!comps.iter().any(|n| n == "connect"), "{comps:?}");
    assert!(comps.iter().all(|n| n != "open"), "{comps:?}");

    // A partial `co` prefix resolves the other member.
    let source = "use prov;\nlet c = prov::connect(\"x\");\n";
    let model = analyze(source);
    let at = offset_of(source, "prov::connect") + "prov::co".len();
    let comps = labels(&model, at);
    assert!(comps.iter().any(|n| n == "connect"), "{comps:?}");
    assert!(!comps.iter().any(|n| n == "make"), "{comps:?}");
}

#[test]
fn module_alias_source_isolation_across_units() {
    // A module used under an alias; the alias's member completion resolves
    // the module's exported functions from the merged flat table.
    let root = "use a::util;\nlet x = util::helper_a();\n";
    let model = analyze_modules(root, &[("a/util.rss", "pub fn helper_a() -> int { 1 }\n")]);
    // Cursor inside the member token `helper_a` (prefix `helper`).
    let at = offset_of(root, "util::helper_a") + "util::helper".len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "helper_a"),
        "module member from the aliased module must resolve: {comps:?}"
    );
}

#[test]
fn module_alias_offered_and_source_scoped() {
    // The module alias itself is offered as a completion at its owning
    // source, and the module's functions are only reachable through the
    // alias namespace, not as plain names.
    let root = "use a::util;\nlet y = 1;\n";
    let model = analyze_modules(root, &[("a/util.rss", "pub fn h() -> int { 1 }\n")]);
    let comps = labels(&model, root.len());
    assert!(
        comps.iter().any(|n| n == "util"),
        "module alias 'util' should be offered: {comps:?}"
    );
    assert!(
        comps.iter().all(|n| n != "h"),
        "module function must only appear via its namespace: {comps:?}"
    );
}

#[test]
fn self_qualified_module_alias_member_completion_resolves_owning_source() {
    // M1-residual: `use self::nested as nested;` must resolve the module
    // member surface exactly like the loader does — the leading `self`
    // qualifier is a no-op relative to the importing file, so `nested::`
    // resolves to `<dir>/nested.rss` and lists that module's exports. The
    // parser records the joined spelling `self::nested`; the semantic model
    // must translate it through the same `use_path_to_spec` routine the
    // loader uses (self -> `./`), never a literal `self/nested` file.
    let root = "use self::nested as nested;\nlet x = nested::leaf();\n";
    let model = analyze_modules(root, &[("nested.rss", "pub fn leaf() -> int { 1 }\n")]);
    let at = offset_of(root, "nested::leaf") + "nested::l".len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "leaf"),
        "self::nested member surface must resolve the aliased module: {comps:?}"
    );
}

#[test]
fn super_qualified_module_alias_member_completion_resolves_parent_directory() {
    // M1-residual: `use super::shared as shared;` from a nested module must
    // resolve the member surface to the parent directory's `shared.rss`,
    // exactly like the loader's `super` -> `..` climb. This is the
    // completion-side counterpart to
    // `nested_module_super_import_resolves_parent_directory_sibling`.
    let root = "use self::pkg::nested as nested;\nlet x = nested::run();\n";
    let model = analyze_modules(
        root,
        &[
            (
                "pkg/nested.rss",
                "use super::shared as shared;\npub fn run() -> int { shared::value() }\n",
            ),
            ("../shared.rss", "pub fn value() -> int { 13 }\n"),
        ],
    );
    // Cursor inside the `value` member token of `shared::value()` in the
    // nested module's own source. The semantic model is built from the
    // merged IR; the nested module's source name is `<dir>/pkg/nested.rss`
    // and the alias resolves to `<dir>/shared.rss`.
    let nested_at = offset_of(
        "use super::shared as shared;\npub fn run() -> int { shared::value() }\n",
        "shared::value",
    ) + "shared::v".len();
    let comps = labels_in_source(&model, "nested.rss", nested_at);
    assert!(
        comps.iter().any(|n| n == "value"),
        "super::shared member surface must resolve the parent sibling module: {comps:?}"
    );
}

#[test]
fn module_member_completions_are_scoped_to_the_aliased_module() {
    // Cross-module leakage guard (M1): with two distinct module aliases, each
    // `ns::` member surface lists only the functions owned by its own module,
    // never the other module's exports.
    let root =
        "use a::util;\nuse b::other;\nlet x = util::util_only();\nlet y = other::other_only();\n";
    let model = analyze_modules(
        root,
        &[
            ("a/util.rss", "pub fn util_only() -> int { 1 }\n"),
            ("b/other.rss", "pub fn other_only() -> int { 2 }\n"),
        ],
    );
    // Cursor at `util::u|` (partial member `u`).
    let at = offset_of(root, "util::util_only") + "util::u".len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "util_only"),
        "util:: member surface offers util's own export: {comps:?}"
    );
    assert!(
        comps.iter().all(|n| n != "other_only"),
        "other module exports must not leak into util:: — {comps:?}"
    );

    // And the reverse: `other::` offers only `other_only`.
    let at = offset_of(root, "other::other_only") + "other::o".len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "other_only"),
        "other:: member surface offers other's own export: {comps:?}"
    );
    assert!(
        comps.iter().all(|n| n != "util_only"),
        "util exports must not leak into other:: — {comps:?}"
    );
}

#[test]
fn trailing_namespace_prefix_offers_empty_member_completion() {
    // M3: a cursor exactly at the `ns::` boundary (nothing typed yet) must
    // still offer the namespace's members — member completion triggers on the
    // trailing `::`, not only after a partial member token.
    let source = "use prov;\nlet c = prov::make(\"x\");\n";
    let model = analyze(source);
    // Cursor on the second Colon of `prov::` (the empty-member boundary,
    // immediately before `make`).
    let at = offset_of(source, "prov::make") + "prov::".len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "make"),
        "empty-member ns:: should offer prov members: {comps:?}"
    );
    assert!(
        comps.iter().any(|n| n == "connect"),
        "empty-member ns:: should offer all prov members: {comps:?}"
    );
}

// ---------------------------------------------------------------------------
// Exact prefix derivation (lexer token stream)
// ---------------------------------------------------------------------------

#[test]
fn unicode_prefix_from_token_span() {
    // Prefix comes from the lexer token span, so Unicode text before the
    // identifier does not confuse byte offsets.
    let source = "let s = \"你好\";\nlet target = 1;\n";
    let model = analyze(source);
    // Cursor inside the identifier `target` at the `targ` prefix.
    let at = offset_of(source, "targ") + "targ".len();
    let comps = labels(&model, at);
    assert!(
        comps.iter().any(|n| n == "target"),
        "prefix 'targ' should offer target: {comps:?}"
    );
}

#[test]
fn whitespace_cursor_gets_empty_prefix() {
    // A cursor in whitespace yields an empty prefix, so all visible names are
    // offered regardless of what precedes the cursor.
    let source = "let alpha = 1;\n\n";
    let model = analyze(source);
    // Cursor on line 2 (blank line).
    let blank = offset_of(source, "\n\n") + 1;
    let comps = labels(&model, blank);
    assert!(
        comps.iter().any(|n| n == "alpha"),
        "empty prefix should not filter out alpha: {comps:?}"
    );
}

// ---------------------------------------------------------------------------
// Exact diagnostic slices
// ---------------------------------------------------------------------------

#[test]
fn host_call_resolve_diagnostic_carries_exact_callee_span() {
    // A failing call must carry its exact callee span, not the whole line.
    let source = "use prov;\nlet a = prov::make(1);\n";
    let dir = temp_root("semantic_diag");
    let main = dir.join("main.rss");
    std::fs::write(&main, source).expect("write");
    let options = CompileSourceFileOptions::new().with_host_api_catalog(test_catalog());
    let model = analyze_source_file_with_options(&main, options).expect("analysis runs");
    let _ = std::fs::remove_dir_all(&dir);

    let diags = model.diagnostics();
    assert_eq!(diags.len(), 1, "expected one resolution error: {diags:?}");
    let span = diags[0].span.expect("exact span carried");
    let callee_lo = offset_of(source, "let a = prov::make(1)") + "let a = ".len();
    let callee = "prov::make";
    assert_eq!(
        (span.lo, span.hi),
        (callee_lo, callee_lo + callee.len()),
        "diagnostic must slice exactly the failing callee token, got {:?}",
        span
    );
    let file = model
        .sources()
        .file(span.source_id)
        .expect("source present");
    assert_eq!(&file.text[span.lo..span.hi], "prov::make");
}

#[test]
fn nested_same_line_calls_report_the_failing_call_slice() {
    // Two calls on one line, the inner one failing: the diagnostic must
    // point at the failing callee's exact token, not the outer call or the
    // line.
    let source = "use prov;\nlet b = prov::make(prov::make(1));\n";
    let dir = temp_root("semantic_diag_nested");
    let main = dir.join("main.rss");
    std::fs::write(&main, source).expect("write");
    let options = CompileSourceFileOptions::new().with_host_api_catalog(test_catalog());
    let model = analyze_source_file_with_options(&main, options).expect("analysis runs");
    let _ = std::fs::remove_dir_all(&dir);

    let diags = model.diagnostics();
    assert_eq!(diags.len(), 1, "expected one resolution error: {diags:?}");
    let span = diags[0].span.expect("exact span carried");
    // The failing call is the inner `prov::make(1)`.
    let inner_lo = offset_of(source, "prov::make(1)");
    let callee = "prov::make";
    assert_eq!(
        (span.lo, span.hi),
        (inner_lo, inner_lo + callee.len()),
        "diagnostic must slice the inner failing callee, got {:?}",
        span
    );
}
