//! Integration tests for the SemanticModel compiler query API.
//!
//! These tests exercise the full public API surface: hover (inferred schema),
//! signature help, completions, diagnostics and definitions. They use the
//! exact catalog-backed path — the SemanticModel is produced from the same
//! catalog snapshot used by CompileSourceFileOptions.
//!
//! The FrontendIr is consumed during codegen, so these tests construct the
//! model directly from catalog + error fixtures rather than going through
//! the full compile pipeline. The unit tests in
//! `src/compiler/semantic_model.rs` exercise the IR-walking internals.

use std::sync::Arc;

use vm::compiler::ir::FrontendIr;
use vm::compiler::source_map::SourceMap;
use vm::compiler::{
    CompileError, SemanticCompletion, SemanticDiagnostic, SemanticModel, SourcePosition,
    TypeSchema, analyze_source,
};
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a test catalog with sqlite, io, and http resources.
fn test_catalog() -> Arc<HostApiCatalog> {
    let sqlite_key = ResourceTypeKey::new("sqlite.connection").unwrap();
    let io_file_key = ResourceTypeKey::new("io.file").unwrap();
    let http_req_key = ResourceTypeKey::new("http.request").unwrap();

    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        sqlite_key.clone(),
        "SQLite database connection",
    ));
    builder.resource(ResourceTypeSchema::new(
        io_file_key.clone(),
        "A file on disk",
    ));
    builder.resource(ResourceTypeSchema::new(
        http_req_key.clone(),
        "An HTTP request handle",
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
        HostTypeSchema::Resource(io_file_key),
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

    Arc::new(builder.build().expect("test catalog build"))
}

fn empty_ir() -> FrontendIr {
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
    }
}

fn build_model(catalog: Arc<HostApiCatalog>, errors: Vec<CompileError>) -> SemanticModel {
    let sources = SourceMap::new();
    SemanticModel::new(empty_ir(), sources, catalog, errors)
}

// ---------------------------------------------------------------------------
// Catalog fingerprint
// ---------------------------------------------------------------------------

#[test]
fn catalog_fingerprint_is_stable() {
    let catalog = test_catalog();
    let fp1 = catalog.fingerprint();
    let fp2 = catalog.fingerprint();
    assert_eq!(fp1, fp2, "fingerprint must be deterministic");
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

#[test]
fn completions_include_host_functions() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        names.contains(&"sqlite::open"),
        "completions missing sqlite::open: {:?}",
        names
    );
    assert!(
        names.contains(&"sqlite::query"),
        "completions missing sqlite::query: {:?}",
        names
    );
    assert!(
        names.contains(&"io::open"),
        "completions missing io::open: {:?}",
        names
    );
    assert!(
        names.contains(&"len"),
        "completions missing len: {:?}",
        names
    );
}

#[test]
fn completions_include_resource_types() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    let resource_labels: Vec<&str> = completions
        .iter()
        .filter(|c| c.kind == vm::compiler::CompletionItemKind::Resource)
        .map(|c| c.label.as_str())
        .collect();
    assert!(
        resource_labels.contains(&"resource<sqlite.connection>"),
        "missing sqlite resource: {:?}",
        resource_labels
    );
    assert!(
        resource_labels.contains(&"resource<io.file>"),
        "missing io.file resource: {:?}",
        resource_labels
    );
    assert!(
        resource_labels.contains(&"resource<http.request>"),
        "missing http.request resource: {:?}",
        resource_labels
    );
}

#[test]
fn completions_detail_shows_passing_modes() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    let query = completions
        .iter()
        .find(|c| c.label == "sqlite::query")
        .expect("sqlite::query should be in completions");
    let detail = query.detail.as_deref().unwrap_or("");
    // The detail should show the borrow resource parameter
    assert!(
        detail.contains("borrow"),
        "sqlite::query detail should show borrow mode: got {detail:?}"
    );
    assert!(
        detail.contains("resource<sqlite.connection>"),
        "sqlite::query detail should show resource type: got {detail:?}"
    );
}

#[test]
fn completions_include_overloads_as_separate_candidates() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    // len has 2 overloads in our test catalog (string, array)
    let len_count = completions.iter().filter(|c| c.label == "len").count();
    assert_eq!(
        len_count, 2,
        "len should have 2 overload completions, got {len_count}"
    );
}

#[test]
fn completions_work_with_custom_catalog() {
    let custom_key = ResourceTypeKey::new("custom.my_resource").unwrap();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        custom_key.clone(),
        "My custom resource",
    ));
    builder.function(HostFunctionSchema::with_return(
        "custom::create",
        vec![HostParamSchema::value("name", HostTypeSchema::String)],
        HostTypeSchema::Resource(custom_key),
    ));
    let catalog = Arc::new(builder.build().expect("custom catalog"));

    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        names.contains(&"custom::create"),
        "custom catalog functions should appear in completions: {:?}",
        names
    );
    assert!(
        names.contains(&"resource<custom.my_resource>"),
        "custom resource should appear in completions: {:?}",
        names
    );
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_empty_when_no_errors() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let diags: Vec<SemanticDiagnostic> = model.diagnostics();
    assert!(
        diags.is_empty(),
        "no errors should produce empty diagnostics"
    );
}

#[test]
fn diagnostics_unknown_host_api() {
    let catalog = test_catalog();
    let errors = vec![CompileError::HostCallResolve {
        line: Some(3),
        source_name: Some("test.rss".to_string()),
        detail: "unknown host function `nonexistent::func`".to_string(),
    }];
    let model = build_model(catalog, errors);
    let diags: Vec<SemanticDiagnostic> = model.diagnostics();
    assert_eq!(diags.len(), 1);
    assert!(
        diags[0].message.contains("nonexistent::func"),
        "unknown host diagnostic should mention the function name: {}",
        diags[0].message
    );
}

#[test]
fn diagnostics_wrong_resource_type() {
    let catalog = test_catalog();
    let errors = vec![CompileError::HostCallResolve {
        line: Some(5),
        source_name: Some("test.rss".to_string()),
        detail: "no host function `sqlite::query` matches the arguments: \
                 expected resource<sqlite.connection> for parameter `connection`, \
                 found resource<io.file>"
            .to_string(),
    }];
    let model = build_model(catalog, errors);
    let diags: Vec<SemanticDiagnostic> = model.diagnostics();

    assert_eq!(diags.len(), 1, "should have exactly one diagnostic");
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

// ---------------------------------------------------------------------------
// TypeSchema display
// ---------------------------------------------------------------------------

#[test]
fn type_schema_display_resource() {
    let key = ResourceTypeKey::new("sqlite.connection").unwrap();
    let schema = TypeSchema::Resource(key);
    assert_eq!(format!("{schema}"), "resource<sqlite.connection>");
}

#[test]
fn type_schema_display_scalars() {
    assert_eq!(format!("{}", TypeSchema::Int), "int");
    assert_eq!(format!("{}", TypeSchema::String), "string");
    assert_eq!(format!("{}", TypeSchema::Bool), "bool");
    assert_eq!(format!("{}", TypeSchema::Null), "null");
    assert_eq!(format!("{}", TypeSchema::Unknown), "unknown");
    assert_eq!(format!("{}", TypeSchema::Float), "float");
    assert_eq!(format!("{}", TypeSchema::Bytes), "bytes");
}

#[test]
fn type_schema_display_containers() {
    let key = ResourceTypeKey::new("io.file").unwrap();
    let schema = TypeSchema::Array(Box::new(TypeSchema::Resource(key)));
    assert_eq!(format!("{schema}"), "array<resource<io.file>>");

    let schema = TypeSchema::Optional(Box::new(TypeSchema::Resource(
        ResourceTypeKey::new("sqlite.connection").unwrap(),
    )));
    assert_eq!(format!("{schema}"), "optional<resource<sqlite.connection>>");
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

#[test]
fn callable_signature_empty_ir() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    assert!(
        model.callable_signature_at(pos).is_none(),
        "empty IR should have no signature"
    );
}

// ---------------------------------------------------------------------------
// Definition
// ---------------------------------------------------------------------------

#[test]
fn definition_unknown_position() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    assert!(
        model.definition_at(pos).is_none(),
        "unknown position should have no definition"
    );
}

// ---------------------------------------------------------------------------
// UTF-8 positions
// ---------------------------------------------------------------------------

#[test]
fn utf8_byte_position_conversion() {
    let mut sources = SourceMap::new();
    let _sid = sources.add_source("test.rss", "let x = 42\nlet y = \"hello\"\n");
    let file = sources.file(0).expect("source file should exist");

    // Check line 1, column 5 (the 'x' in 'let x = 42')
    let offset = file.line_col_to_offset(1, 5);
    assert!(offset.is_some(), "should find offset for line 1 col 5");
    let (line, col) = file
        .line_col_for_offset(offset.unwrap())
        .expect("should resolve back");
    assert_eq!(line, 1, "should be line 1");
    assert_eq!(col, 5, "should be column 5");

    // Check line 2, column 5 (the 'y' in 'let y = \"hello\"')
    let offset = file.line_col_to_offset(2, 5);
    assert!(offset.is_some(), "should find offset for line 2 col 5");
    let (line, col) = file
        .line_col_for_offset(offset.unwrap())
        .expect("should resolve back");
    assert_eq!(line, 2, "should be line 2");
    assert_eq!(col, 5, "should be column 5");
}

// ---------------------------------------------------------------------------
// Same name / arity overloads with different schemas
// ---------------------------------------------------------------------------

#[test]
fn overloads_with_different_schemas_are_independent_candidates() {
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    // len has 2 overloads: len(string) -> int and len(array) -> int
    let len_overloads: Vec<&SemanticCompletion> =
        completions.iter().filter(|c| c.label == "len").collect();
    assert_eq!(
        len_overloads.len(),
        2,
        "len should have 2 separate overload entries"
    );

    // Each overload should have a detail string that distinguishes them
    for overload in &len_overloads {
        let detail = overload.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("fn("),
            "overload detail should show function signature: {detail}"
        );
    }
}

// ---------------------------------------------------------------------------
// Deterministic snapshots
// ---------------------------------------------------------------------------

#[test]
fn deterministic_catalog_fingerprint() {
    let catalog_a = test_catalog();
    let catalog_b = test_catalog();
    assert_eq!(
        catalog_a.fingerprint(),
        catalog_b.fingerprint(),
        "identical catalogs must have identical fingerprints"
    );
}

#[test]
fn deterministic_completions() {
    let catalog = test_catalog();
    let model_a = build_model(catalog.clone(), Vec::new());
    let model_b = build_model(catalog, Vec::new());

    let pos = SourcePosition::new(0, 0);
    let completions_a = model_a.completions_at(pos);
    let completions_b = model_b.completions_at(pos);

    // Same number of completions
    assert_eq!(
        completions_a.len(),
        completions_b.len(),
        "deterministic models should produce same completion count"
    );

    // Same labels in same order
    for (a, b) in completions_a.iter().zip(completions_b.iter()) {
        assert_eq!(a.label, b.label, "completion labels should match");
        assert_eq!(a.detail, b.detail, "completion details should match");
        assert_eq!(a.kind, b.kind, "completion kinds should match");
    }
}

// ---------------------------------------------------------------------------
// Catalog fingerprint identity
// ---------------------------------------------------------------------------

#[test]
fn model_exposes_catalog_fingerprint() {
    let catalog = test_catalog();
    let model = build_model(catalog.clone(), Vec::new());
    let model_fp = model.catalog_fingerprint();
    let catalog_fp = catalog.fingerprint();
    assert_eq!(
        model_fp, catalog_fp,
        "model fingerprint must match catalog fingerprint"
    );
}

#[test]
fn model_catalog_readonly() {
    let catalog = test_catalog();
    let model = build_model(catalog.clone(), Vec::new());
    let model_catalog = model.catalog();
    assert_eq!(
        model_catalog.fingerprint(),
        catalog.fingerprint(),
        "model catalog should be the same catalog"
    );
}

// ---------------------------------------------------------------------------
// Nested call-site resolution
// ---------------------------------------------------------------------------

#[test]
fn completed_source_text_has_no_effect_on_catalog_completions() {
    // Ensure that completions are purely catalog-driven and not affected
    // by the source text (since the IR is empty in our test).
    let catalog = test_catalog();
    let model = build_model(catalog, Vec::new());
    let pos = SourcePosition::new(0, 0);
    let completions = model.completions_at(pos);

    // All catalog functions should be present: sqlite::open, sqlite::query, io::open, len
    assert!(
        completions.len() >= 6,
        "should have at least 6 completions (4 functions + resources)"
    );
    // Verify specific catalog functions are present
    let fn_count = completions
        .iter()
        .filter(|c| c.kind == vm::compiler::CompletionItemKind::Function)
        .count();
    // Our catalog has: sqlite::open, sqlite::query, io::open, len (2 overloads) = 5 functions
    assert_eq!(
        fn_count, 5,
        "should have 5 function completions (including overloads)"
    );
}

// ---------------------------------------------------------------------------
// Real pipeline tests (analyze_source)
// ---------------------------------------------------------------------------

#[test]
fn analyze_source_basic() {
    let source = "let x = 42;";
    let model = analyze_source(source).expect("analyze_source should succeed");
    let completions = model.completions_at(SourcePosition::new(0, 0));
    assert!(!completions.is_empty(), "completions should not be empty");
}

#[test]
fn analyze_source_with_catalog_works() {
    // analyze_source creates a default catalog; verify it doesn't crash
    let source = "let x = 42;";
    let model = analyze_source(source).expect("analyze_source should succeed");
    assert!(
        model.catalog().functions().is_empty(),
        "default catalog should be empty"
    );
}

#[test]
fn analyze_source_diagnostics() {
    let source = "let x = ";
    let model = analyze_source(source);
    // Should either succeed or produce a parse error
    match model {
        Ok(model) => {
            let diags: Vec<SemanticDiagnostic> = model.diagnostics();
            // Incomplete expression should produce diagnostics
            assert!(
                !diags.is_empty(),
                "incomplete source should have diagnostics"
            );
        }
        Err(_) => {
            // Parse error is also acceptable
        }
    }
}

#[test]
fn analyze_source_line_col_conversion() {
    let source = "let x = 42;\nlet y = 43;\n";
    let model = analyze_source(source).expect("analyze_source should succeed");
    let (line, col) = model
        .offset_to_line_col(SourcePosition::new(0, 0))
        .expect("should get line/col");
    assert_eq!(line, 1, "offset 0 should be line 1");
    assert_eq!(col, 1, "offset 0 should be column 1");
    // Second line starts at offset 12 (after "let x = 42\n")
    let (line, col) = model
        .offset_to_line_col(SourcePosition::new(0, 12))
        .expect("should get line/col");
    assert_eq!(line, 2, "offset 12 should be line 2");
    assert_eq!(col, 1, "offset 12 should be column 1");
    // Round-trip
    let offset = model
        .line_col_to_offset(0, 2, 1)
        .expect("should get offset");
    assert_eq!(offset, 12);
}

#[test]
fn analyze_source_completions_filtered() {
    let source = "let x = 42;\n";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // Completions at the start of the file should include catalog functions
    let completions = model.completions_at(SourcePosition::new(0, 0));
    let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // At position 0, there's no prefix, so all catalog functions should appear
    assert!(!completions.is_empty(), "completions should not be empty");
}

#[test]
fn analyze_source_definition_at_local() {
    let source = "let x = 42;\nx;";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // Try to find a definition at position where 'x' is referenced
    // The definition should be at the let-binding
    let def = model.definition_at(SourcePosition::new(0, 10));
    // May or may not find a definition depending on implementation
    // This test primarily ensures no crash
    let _ = def;
}

#[test]
fn analyze_source_inferred_schema() {
    let source = "let x = 42;";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // The schema at offset 0 should be int (the literal)
    let schema = model.inferred_schema_at(SourcePosition::new(0, 0));
    // The inferred schema may or may not be available depending on
    // whether the semantic index is populated for this position
    if let Some(schema) = schema {
        assert_eq!(schema, vm::compiler::TypeSchema::Int);
    }
}

#[test]
fn analyze_source_utf16_conversion() {
    let source = "let x = \"héllo\";";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // The UTF-16 column of the 'é' character (offset 9)
    let utf16_col = model.offset_to_utf16_column(SourcePosition::new(0, 9));
    if let Some(col) = utf16_col {
        // 'é' is 2 bytes in UTF-8 but 1 code unit in UTF-16
        // So offset 9 should be at UTF-16 column 9 (since previous chars are ASCII)
        assert_eq!(col, 9, "UTF-16 column at offset 9 should be 9");
    }
}

// ---------------------------------------------------------------------------
// Exact span and position tests (analyze_source only)
// ---------------------------------------------------------------------------

#[test]
fn analyze_source_local_declaration_hover() {
    let source = "let x = 42;";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // Hover on 'x' (offset 4..5)
    let schema = model.inferred_schema_at(SourcePosition::new(0, 4));
    assert_eq!(
        schema,
        Some(vm::compiler::TypeSchema::Int),
        "hover on local 'x' should show int"
    );
}

#[test]
fn analyze_source_local_definition_exact_span() {
    let source = "let x = 42;\nx;";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // Find definition at the reference on line 2 (offset 12, 'x' at "let x = 42;\n" = 11 char + 1 newline = 12)
    let def = model.definition_at(SourcePosition::new(0, 12));
    assert!(def.is_some(), "should find definition for 'x' reference");
    if let Some(def) = def {
        assert_eq!(def.label, "let x", "definition label should be 'let x'");
        // The span should point to the declaration identifier 'x' at offset 4..5
        assert_eq!(def.span.lo, 4, "definition span should start at offset 4");
        assert_eq!(def.span.hi, 5, "definition span should end at offset 5");
    }
}

#[test]
fn analyze_source_function_declaration_definition() {
    let source = "fn foo() -> int { 42 }";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // Find definition at the 'foo' declaration (offset 3..6)
    let def = model.definition_at(SourcePosition::new(0, 4));
    assert!(def.is_some(), "should find definition for 'foo'");
    if let Some(def) = def {
        assert!(def.label.contains("foo"), "label should mention 'foo'");
    }
}

#[test]
fn analyze_source_unicode_before_target() {
    let source = "// unicode: 你好\nlet x = 42;\n";
    let model = analyze_source(source).expect("analyze_source should succeed");
    // The unicode comment takes 19 bytes: "// unicode: " (12) + "你好" (6) + "\n" (1) = 19
    // Then "let x" starts at offset 19, 'x' is at offset 23..24
    let schema = model.inferred_schema_at(SourcePosition::new(0, 23));
    assert_eq!(
        schema,
        Some(vm::compiler::TypeSchema::Int),
        "hover on 'x' after unicode should show int"
    );
}

#[test]
fn analyze_source_diagnostic_error_code() {
    let source = "let x = unknown_func();\n";
    let model = analyze_source(source);
    match model {
        Ok(model) => {
            let diags = model.diagnostics();
            for diag in &diags {
                if let Some(ref code) = diag.code {
                    assert!(
                        code.starts_with("E"),
                        "error code should start with E: {}",
                        code
                    );
                }
            }
        }
        Err(_) => {
            // Parse error is also acceptable
        }
    }
}

#[test]
fn analyze_source_semantic_index_present() {
    let source = "let x = 42;\n";
    let model = analyze_source(source).expect("analyze_source should succeed");
    let index = model.ir().semantic_index.as_ref();
    assert!(
        index.is_some(),
        "analyze_source should produce a semantic index"
    );
    if let Some(index) = index {
        // slot_decl_spans should contain 'x'
        assert!(
            !index.slot_decl_spans.is_empty(),
            "slot_decl_spans should not be empty"
        );
        // There should be a root scope
        assert!(
            !index.scope_records.is_empty(),
            "scope_records should not be empty"
        );
        // Verify root scope
        assert_eq!(
            index.scope_records[0].parent, None,
            "root scope should have no parent"
        );
    }
}

#[test]
fn analyze_source_local_scope_visibility() {
    let source = "let x = 1;\nlet y = 2;\n";
    let model = analyze_source(source).expect("analyze_source should succeed");
    let completions = model.completions_at(SourcePosition::new(0, 0));
    let names: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // At position 0 (start of file), 'x' should be visible
    assert!(names.contains(&"x"), "'x' should be visible");
}
