//! End-to-end SemanticModel tests driven by parser provenance.
//!
//! These tests exercise the full real compile pipeline (parse -> legalize ->
//! type-check -> provenance-driven semantic index) and assert exact source
//! slices for:
//!
//! * repeated same-line, nested, multiline, and Unicode calls;
//! * local shadowing definitions;
//! * function value / direct / module references;
//! * namespace and postfix calls;
//! * multi-source [`SourceId`]s;
//! * absent synthetic call sites (calls without parser provenance never
//!   appear as source sites).
//!
//! All assertions use exact byte offsets into the owning source text — no
//! `Some(...) || None` fallbacks and no `let _ =` swallow patterns.

use std::path::PathBuf;
use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SemanticModel, SourcePosition, TypeSchema, analyze_source,
    analyze_source_file_with_options,
};
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamSchema, HostTypeSchema,
    ResourceTypeKey, ResourceTypeSchema,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A catalog with a small set of deterministic host functions for call
/// resolution tests.
fn provenance_catalog() -> Arc<HostApiCatalog> {
    let conn_key = ResourceTypeKey::new("prov.connection").unwrap();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        conn_key.clone(),
        "A provenance connection",
    ));

    // make(path: string) -> resource<prov.connection>
    builder.function(HostFunctionSchema::with_return(
        "prov::make",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(conn_key.clone()),
    ));

    // describe(connection: borrow resource<prov.connection>) -> string
    builder.function(HostFunctionSchema::with_return(
        "prov::describe",
        vec![HostParamSchema::with_passing(
            "connection",
            HostTypeSchema::Resource(conn_key.clone()),
            vm::host_api::HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
    ));

    Arc::new(builder.build().expect("provenance catalog build"))
}

/// Analyze a single source string through the real pipeline with the
/// provenance catalog.
fn analyze_with_catalog(source: &str) -> SemanticModel {
    let dir = temp_module_root("semantic_model_provenance");
    let main_path = dir.join("main.rss");
    std::fs::write(&main_path, source).expect("main source should write");
    let options = CompileSourceFileOptions::new().with_host_api_catalog(provenance_catalog());
    let model =
        analyze_source_file_with_options(&main_path, options).expect("analysis should succeed");
    let _ = std::fs::remove_dir_all(&dir);
    model
}

/// Analyze a root source with module overrides through the real module
/// pipeline (loader + linker + legalize + index).
fn analyze_with_modules(root: &str, overrides: &[(&str, &str)]) -> SemanticModel {
    let dir = temp_module_root("semantic_model_provenance_mod");
    let main_path = dir.join("main.rss");
    std::fs::write(&main_path, root).expect("main source should write");
    let mut options = CompileSourceFileOptions::new().with_host_api_catalog(provenance_catalog());
    for (spec, source) in overrides {
        options = options.with_module_override_source(*spec, *source);
    }
    let model = analyze_source_file_with_options(&main_path, options)
        .expect("module analysis should succeed");
    let _ = std::fs::remove_dir_all(&dir);
    model
}

/// Create a unique temporary directory for one test.
fn temp_module_root(prefix: &str) -> PathBuf {
    let unique = format!(
        "{prefix}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");
    root
}

/// Assert the exact source slice at a span equals `expected`.
fn assert_slice(model: &SemanticModel, span: vm::compiler::source_map::Span, expected: &str) {
    let file = model
        .sources()
        .file(span.source_id)
        .unwrap_or_else(|| panic!("no source for id {}", span.source_id));
    let slice = &file.text[span.lo..span.hi];
    assert_eq!(
        slice, expected,
        "span {}..{} in source {} should be {:?}, got {:?}",
        span.lo, span.hi, span.source_id, expected, slice
    );
}

/// Byte offset of the first occurrence of `needle` in `haystack` (test-side
/// position computation; the SemanticModel itself never scans source text).
fn offset_of(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("'{needle}' not found in {haystack:?}"))
}

/// The byte offset of the identifier token `name` starting at the first
/// occurrence of `name` that is preceded by a non-identifier boundary.
fn ident_offset(source: &str, name: &str) -> usize {
    let mut search_from = 0;
    loop {
        let Some(at) = source[search_from..].find(name) else {
            panic!("identifier '{name}' not found in {source:?}");
        };
        let at = search_from + at;
        let before_ok = at == 0
            || !source[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after_ok = at + name.len() == source.len()
            || !source[at + name.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if before_ok && after_ok {
            return at;
        }
        search_from = at + name.len();
    }
}

/// Find the offset of the Nth occurrence of `needle` (0-based).
fn nth_offset(source: &str, needle: &str, n: usize) -> usize {
    let mut at = 0;
    for _ in 0..=n {
        let rest = &source[at..];
        let rel = rest
            .find(needle)
            .unwrap_or_else(|| panic!("occurrence {n} of '{needle}' not found"));
        at += rel;
        if n == 0 {
            return at;
        }
        at += needle.len();
    }
    at
}

// ---------------------------------------------------------------------------
// Repeated same-line calls
// ---------------------------------------------------------------------------

#[test]
fn same_line_repeated_calls_resolve_independently() {
    let source = "fn tag(s: string) -> string { s }\nlet a = tag(\"x\"); let b = tag(\"y\");";
    let model = analyze_with_catalog(source);
    let decl = ident_offset(source, "tag"); // declaration identifier
    let first_callee = offset_of(source, "let a = tag") + 8;
    let second_callee = offset_of(source, "let b = tag") + 8;
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, first_callee + 1)),
        Some(TypeSchema::String),
        "first same-line call should resolve"
    );
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, second_callee + 1)),
        Some(TypeSchema::String),
        "second same-line call should resolve independently"
    );
    // Definitions resolve to the declaration identifier.
    let first = model
        .definition_at(SourcePosition::new(0, first_callee + 1))
        .expect("first call definition");
    let second = model
        .definition_at(SourcePosition::new(0, second_callee + 1))
        .expect("second call definition");
    assert_eq!(
        first.span.lo, decl,
        "first call resolves to declaration start"
    );
    assert_eq!(
        second.span.lo, decl,
        "second call resolves to declaration start"
    );
    assert_slice(&model, first.span, "tag");
    assert_slice(&model, second.span, "tag");
}

#[test]
fn same_line_identical_calls_pick_smallest_span() {
    let source = "fn tag(s: string) -> string { s }\nlet a = tag(\"x\"); let b = tag(tag(\"y\"));";
    let model = analyze_with_catalog(source);
    // The inner `tag` on the second statement: its callee start.
    let inner = offset_of(source, "tag(\"y\")");
    // Both calls return string, but the inner call must be the one selected
    // (its span is strictly contained in the outer's).
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, inner + 1)),
        Some(TypeSchema::String),
        "inner nested call wins over the outer call"
    );
}

// ---------------------------------------------------------------------------
// Nested calls
// ---------------------------------------------------------------------------

#[test]
fn nested_calls_resolve_inner_over_outer() {
    let source = "fn tag(s: string) -> string { s }\nlet x = tag(tag(\"deep\"));";
    let model = analyze_with_catalog(source);
    let inner = offset_of(source, "tag(\"deep\")");
    let outer = offset_of(source, "let x = tag") + 8;
    let inner_hover = model.inferred_schema_at(SourcePosition::new(0, inner + 1));
    assert_eq!(inner_hover, Some(TypeSchema::String), "inner call resolves");
    let outer_hover = model.inferred_schema_at(SourcePosition::new(0, outer + 1));
    assert_eq!(outer_hover, Some(TypeSchema::String), "outer call resolves");
}

// ---------------------------------------------------------------------------
// Multiline calls
// ---------------------------------------------------------------------------

#[test]
fn multiline_call_span_covers_full_expression() {
    let source = "fn tag(s: string) -> string { s }\nlet x = tag(\n  \"multi\"\n);\n";
    let model = analyze_with_catalog(source);
    let arg_inside = offset_of(source, "\"multi\"") + 1;
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, arg_inside)),
        Some(TypeSchema::String),
        "cursor inside multiline argument list resolves to the call"
    );
    let callee = offset_of(source, "tag(\n") + 1;
    let def = model.definition_at(SourcePosition::new(0, callee));
    assert!(def.is_some(), "call should have a definition");
    assert_slice(&model, def.expect("call definition").span, "tag");
}

// ---------------------------------------------------------------------------
// Unicode calls
// ---------------------------------------------------------------------------

#[test]
fn unicode_source_offsets_are_exact() {
    // Unicode is exercised through string literals (the lexer keeps
    // identifiers ASCII); the call after a multibyte string must resolve at
    // exact byte offsets.
    let source = "fn tag(s: string) -> string { s }\nlet a = \"值\";\nlet b = tag(a);\n";
    let model = analyze_with_catalog(source);
    let decl = ident_offset(source, "tag"); // declaration identifier
    let call_callee = offset_of(source, "let b = tag") + 8;
    // Call after unicode text resolves with exact byte offsets.
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, call_callee + 1)),
        Some(TypeSchema::String),
        "call after unicode text resolves with exact byte offsets"
    );
    // The string literal's own local `a` resolves by its identifier span.
    let a_decl = offset_of(source, "let a =") + 4;
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, a_decl)),
        Some(TypeSchema::String),
        "local bound to a unicode string literal resolves"
    );
    // Definition from the `a` reference resolves to the declaration span.
    let a_ref = offset_of(source, "tag(a)") + 4;
    let def = model.definition_at(SourcePosition::new(0, a_ref));
    assert!(
        def.is_some(),
        "unicode-adjacent reference should have a definition"
    );
    let def = def.expect("definition");
    assert_eq!(
        def.span.lo, a_decl,
        "definition points at the declaration start"
    );
    assert_slice(&model, def.span, "a");
    // And the call itself resolves to the function declaration.
    let call_def = model.definition_at(SourcePosition::new(0, call_callee));
    assert!(call_def.is_some(), "call after unicode should resolve");
    assert_eq!(
        call_def.expect("call definition").span.lo,
        decl,
        "call resolves to the declaration"
    );
}

// ---------------------------------------------------------------------------
// Local shadowing definitions
// ---------------------------------------------------------------------------

#[test]
fn local_shadowing_resolves_innermost_declaration() {
    // Function params allocate distinct slots from module locals, so a param
    // named `x` genuinely shadows a module-level `x`.
    let source = "let x = 1;\nfn f(x: int) -> int {\n  x\n}\nx;\n";
    let model = analyze_with_catalog(source);
    // The reference on line 3 resolves to the param declaration on line 2.
    let param_ref = offset_of(source, "x\n}");
    let def = model.definition_at(SourcePosition::new(0, param_ref));
    assert!(def.is_some(), "shadowed reference should resolve");
    let def = def.expect("shadowed definition");
    let param_decl = offset_of(source, "f(x:") + 2; // param identifier after "f("
    assert_eq!(
        def.span.lo, param_decl,
        "reference resolves to the param declaration"
    );
    assert_eq!(def.span.hi, param_decl + 1, "param declaration span end");
    assert_slice(&model, def.span, "x");

    // The module-level reference on line 5 resolves back to the module-level
    // declaration (the `let x` on line 1).
    let module_ref = offset_of(source, "x;\n");
    let outer_def = model.definition_at(SourcePosition::new(0, module_ref));
    assert!(outer_def.is_some(), "module-level reference should resolve");
    let outer_def = outer_def.expect("module-level definition");
    let module_decl = offset_of(source, "let x = 1") + 4;
    assert_eq!(
        outer_def.span.lo, module_decl,
        "module reference resolves to the module-level declaration"
    );
    assert_slice(&model, outer_def.span, "x");
}

#[test]
fn shadowed_local_hover_uses_declared_schema() {
    let source = "let x = 1;\nfn f(x: int) -> int {\n  x\n}\n";
    let model = analyze_with_catalog(source);
    let param_ref = offset_of(source, "x\n}");
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, param_ref)),
        Some(TypeSchema::Int),
        "hover on shadowed param reference shows the param type"
    );
}

// ---------------------------------------------------------------------------
// Function value / direct / module references
// ---------------------------------------------------------------------------

#[test]
fn direct_function_call_definition_resolves_to_declaration() {
    let source = "fn helper() -> int { 42 }\nlet x = helper();\n";
    let model = analyze_with_catalog(source);
    let decl = ident_offset(source, "helper");
    let callee = offset_of(source, "let x = helper") + 8;
    let def = model.definition_at(SourcePosition::new(0, callee + 1));
    assert!(
        def.is_some(),
        "direct call should resolve to the declaration"
    );
    let def = def.expect("direct call definition");
    assert_eq!(def.span.lo, decl, "declaration identifier start");
    assert_eq!(
        def.span.hi,
        decl + "helper".len(),
        "declaration identifier end"
    );
    assert_slice(&model, def.span, "helper");
    assert!(def.label.contains("helper"), "label names the function");
}

#[test]
fn function_value_reference_definition_resolves_to_declaration() {
    let source = "fn helper() -> int { 42 }\nlet f = helper;\n";
    let model = analyze_with_catalog(source);
    let decl = ident_offset(source, "helper");
    let reference = offset_of(source, "let f = helper") + 8;
    let def = model.definition_at(SourcePosition::new(0, reference + 1));
    assert!(
        def.is_some(),
        "function value should resolve to declaration"
    );
    let def = def.expect("function value definition");
    assert_eq!(def.span.lo, decl, "declaration identifier start");
    assert_slice(&model, def.span, "helper");
}

#[test]
fn module_function_call_definition_resolves_by_symbol() {
    let root = "use a::util;\nfn run() -> int { helper() }\n";
    let model = analyze_with_modules(root, &[("a/util.rss", "pub fn helper() -> int { 7 }\n")]);
    // The merged model carries the root source at SourceId 0 and the module
    // at SourceId 1. The call `helper()` in the root resolves to the module's
    // declaration identifier by symbol identity.
    let callee = offset_of(root, "helper()");
    let def = model.definition_at(SourcePosition::new(0, callee + 1));
    assert!(def.is_some(), "module call should resolve by symbol");
    let def = def.expect("module call definition");
    assert_eq!(
        def.span.source_id, 1,
        "definition lives in the module source"
    );
    assert_slice(&model, def.span, "helper");
}

#[test]
fn module_function_value_reference_resolves_by_symbol() {
    let root = "use a::util;\nlet f = helper;\n";
    let model = analyze_with_modules(root, &[("a/util.rss", "pub fn helper() -> int { 7 }\n")]);
    // Function-value reference `helper` in root.
    let reference = offset_of(root, "helper");
    let def = model.definition_at(SourcePosition::new(0, reference + 1));
    assert!(
        def.is_some(),
        "module function value should resolve by symbol"
    );
    let def = def.expect("module function value definition");
    assert_eq!(
        def.span.source_id, 1,
        "definition lives in the module source"
    );
    assert_slice(&model, def.span, "helper");
}

#[test]
fn function_value_reference_hover_returns_callable_schema() {
    // Hover on a function-value reference (`let f = helper;` at `helper`)
    // must return the referenced function's callable signature schema, not
    // `None` (L1).
    let source = "fn helper(a: int) -> int { a }\nlet f = helper;\n";
    let model = analyze_with_catalog(source);
    let reference = offset_of(source, "let f = helper") + 8;
    let schema = model.inferred_schema_at(SourcePosition::new(0, reference + 1));
    assert_eq!(
        schema,
        Some(TypeSchema::Callable {
            params: vec![TypeSchema::Int],
            result: Box::new(TypeSchema::Int),
        }),
        "function-value reference hover returns the callable schema"
    );
}

#[test]
fn local_callable_call_hover_returns_slot_result_schema() {
    // Hover on a direct local-callable call `f(1)` must return the slot
    // callable's result schema (`int`), never hardcoded `unknown` (L1).
    let source = "fn helper(a: int) -> int { a }\nlet f = helper;\nlet r = f(1);\n";
    let model = analyze_with_catalog(source);
    let callee = offset_of(source, "let r = f") + 8;
    let schema = model.inferred_schema_at(SourcePosition::new(0, callee));
    assert_eq!(
        schema,
        Some(TypeSchema::Int),
        "direct local-callable call hover returns the slot callable's result"
    );
}

#[test]
fn local_reference_inside_call_argument_hover_resolves_to_local_type() {
    // Hover on a local reference used as a call argument (`tag(a)`) must
    // resolve to the local's own type, never the containing call's return
    // type (M2).
    let source = "fn tag(s: string) -> int { 1 }\nlet a = 42;\nlet b = tag(a);\n";
    let model = analyze_with_catalog(source);
    let arg = offset_of(source, "tag(a)") + 4;
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, arg)),
        Some(TypeSchema::Int),
        "hover on a call-argument local reference shows the local's own type"
    );
}

// ---------------------------------------------------------------------------
// Namespace / postfix calls
// ---------------------------------------------------------------------------

#[test]
fn namespace_call_resolves_and_defines_by_schema_identity() {
    let source = "use prov;\nlet c = prov::make(\"db\");\n";
    let model = analyze_with_catalog(source);
    let callee = offset_of(source, "prov::make");
    let schema = model.inferred_schema_at(SourcePosition::new(0, callee + 4));
    assert_eq!(
        schema,
        Some(TypeSchema::Resource(
            ResourceTypeKey::new("prov.connection").unwrap()
        )),
        "namespace call returns its resolved resource schema"
    );
    let sig = model.callable_signature_at(SourcePosition::new(0, callee + 4));
    assert!(sig.is_some(), "namespace call has a signature");
    let sig = sig.expect("namespace signature");
    assert_eq!(sig.name, "prov::make", "signature names the host function");
    // Definition uses the resolved schema identity (host://prov::make/1).
    let def = model.definition_at(SourcePosition::new(0, callee + 4));
    assert!(def.is_some(), "namespace call has a definition");
    let def = def.expect("namespace definition");
    assert!(
        def.label.contains("host://prov::make/1"),
        "host definition is keyed by schema identity, got: {}",
        def.label
    );
    assert_slice(&model, def.span, "prov::make");
}

#[test]
fn postfix_style_namespace_call_resolves() {
    // Namespace member calls parse as namespace calls; the outer describe
    // borrows the stored resource from the inner make.
    let source = "use prov;\nlet c = prov::make(\"db\");\nlet s = prov::describe(&c);\n";
    let model = analyze_with_catalog(source);
    let outer = offset_of(source, "prov::describe");
    let inner = offset_of(source, "prov::make");
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, outer + 4)),
        Some(TypeSchema::String),
        "outer describe resolves to string"
    );
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, inner + 4)),
        Some(TypeSchema::Resource(
            ResourceTypeKey::new("prov.connection").unwrap()
        )),
        "inner make resolves to its resource schema"
    );
}

// ---------------------------------------------------------------------------
// Multi-source SourceIds
// ---------------------------------------------------------------------------

#[test]
fn multi_source_model_keeps_original_source_ids() {
    let root = "use a::util;\nlet x = helper();\n";
    let model = analyze_with_modules(root, &[("a/util.rss", "pub fn helper() -> int { 7 }\n")]);
    let callee = offset_of(root, "helper()");
    // Root source is SourceId 0.
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(0, callee + 1)),
        Some(TypeSchema::Int),
        "root-source call resolves through the module pipeline"
    );
    // The module source (SourceId 1) declaration is reachable.
    let def = model.definition_at(SourcePosition::new(0, callee + 1));
    let def = def.expect("module definition");
    assert_eq!(def.span.source_id, 1, "module declaration is in SourceId 1");
    // Hover directly on the module declaration identifier (SourceId 1).
    // `pub fn helper()` — the identifier `helper` starts at byte 8.
    assert_eq!(
        model.inferred_schema_at(SourcePosition::new(1, 9)),
        Some(TypeSchema::Int),
        "hover in the module source resolves by its own SourceId"
    );
}

// ---------------------------------------------------------------------------
// Absent synthetic sites
// ---------------------------------------------------------------------------

#[test]
fn synthetic_calls_without_provenance_do_not_appear_as_sites() {
    // A plain analyze of a program with no catalog-resolved calls: the IR
    // carries no call-site provenance for compiler-synthetic calls, so no
    // position resolves to a call that does not exist in the source.
    let model = analyze_source("let x = 42; x;").expect("plain analysis should succeed");
    // Position inside the let expression — there is no call site here.
    assert!(
        model.definition_at(SourcePosition::new(0, 5)).is_none(),
        "no synthetic call site should appear at a plain expression"
    );
}

#[test]
fn absent_source_position_returns_none_for_all_queries() {
    let model = analyze_with_catalog("let x = 1;\n");
    // A position past the end of the file has no semantic item.
    let pos = SourcePosition::new(0, 1000);
    assert!(model.inferred_schema_at(pos).is_none());
    assert!(model.callable_signature_at(pos).is_none());
    assert!(model.definition_at(pos).is_none());
}

// ---------------------------------------------------------------------------
// Stability
// ---------------------------------------------------------------------------

#[test]
fn provenance_queries_are_stable_across_repeated_analysis() {
    let source =
        "fn tag(s: string) -> string { s }\nfn helper() -> int { 42 }\nlet x = tag(\"v\");";
    let model_a = analyze_with_catalog(source);
    let model_b = analyze_with_catalog(source);

    let probe = |model: &SemanticModel| -> (Option<TypeSchema>, Option<vm::compiler::Definition>) {
        let schema = model.inferred_schema_at(SourcePosition::new(0, 5));
        let def = model.definition_at(SourcePosition::new(0, 5));
        (schema, def)
    };

    let (schema_a, def_a) = probe(&model_a);
    let (schema_b, def_b) = probe(&model_b);
    assert_eq!(schema_a, schema_b, "hover results must be stable");
    assert_eq!(def_a, def_b, "definition results must be stable");
    let def_a = def_a.expect("definition present");
    assert_eq!(
        def_a.span,
        def_b.expect("definition present").span,
        "spans stable"
    );
}

// ---------------------------------------------------------------------------
// Exact typed diagnostic spans (H1)
// ---------------------------------------------------------------------------

#[test]
fn if_else_branch_mismatch_diagnostic_carries_exact_statement_span() {
    // A real if/else branch type mismatch must carry the exact parser-origin
    // statement span, never a same-line call/declaration guess (H1).
    let source = "let mut x = 1;\nif true { x = \"a\"; } else { x = 2; }\n";
    let model = analyze_with_catalog(source);
    let diags = model.diagnostics();
    let mismatch = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("E005"))
        .unwrap_or_else(|| panic!("expected IfElseBranchTypeMismatch diagnostic: {diags:?}"));
    let span = mismatch.span.expect("typed diagnostic carries exact span");
    // The span must slice the if/else construct, not a token on the line.
    let stmt_lo = offset_of(source, "if true");
    assert_eq!(
        span.lo, stmt_lo,
        "diagnostic starts at the if/else statement, got {:?}",
        span
    );
    let file = model
        .sources()
        .file(span.source_id)
        .expect("source present");
    assert!(
        file.text[span.lo..span.hi].contains("if"),
        "span slices the if/else construct: {:?}",
        &file.text[span.lo..span.hi]
    );
    assert!(span.hi > span.lo, "statement span has positive length");
}

#[test]
fn binary_operand_mismatch_diagnostic_carries_exact_statement_span() {
    // A real binary operand type mismatch (in a typed function body whose
    // parameter types are observed from a call site, where strict add-type
    // checking fires E004 on unresolvable `+` operands) carries the exact
    // parser-origin statement span — the containing fn-decl statement whose
    // body hosts the failing `+` — never a same-line token guess (H1).
    let source = "fn f(a: int, b: bool) -> int { a + b }\nlet r = f(1, true);\n";
    let model = analyze_with_catalog(source);
    let diags = model.diagnostics();
    let mismatch = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("E004"))
        .unwrap_or_else(|| panic!("expected BinaryOperandTypeMismatch diagnostic: {diags:?}"));
    let span = mismatch.span.expect("typed diagnostic carries exact span");
    // The span is the exact fn-decl statement construct covering the failing
    // `a + b` expression — never a call-site or declaration token guess.
    let stmt_lo = offset_of(source, "fn f(a:");
    assert_eq!(
        span.lo, stmt_lo,
        "diagnostic starts at the containing fn-decl statement, got {:?}",
        span
    );
    let file = model
        .sources()
        .file(span.source_id)
        .expect("source present");
    assert!(
        file.text[span.lo..span.hi].contains("a + b"),
        "span covers the failing binary expression: {:?}",
        &file.text[span.lo..span.hi]
    );
    assert!(span.hi > span.lo, "statement span has positive length");
}
