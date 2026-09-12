//! Catalog-free parser fallback must not reserve the full standard catalog.
//!
//! Public [`parse_source_with_dialect`], import scan, and parses without a
//! catalog snapshot install HTTP named structs only when the HTTP surface is
//! available. Unrelated names such as `JitConfig` and `SqliteLimits` stay
//! unknown unless an explicit catalog provides them.

use std::sync::Arc;

use vm::{
    CompileSourceFileOptions, FrontendIr, HostApiBuilder, HostApiCatalog, HostFunctionSchema,
    HostStructField, HostStructSchema, HostTypeSchema, ParseError, ParserDialect,
    SharedParserOptions, SourceFlavor, compile_source_with_flavor_and_options,
    parse_source_with_dialect,
};

#[cfg(not(all(feature = "http-client", not(target_family = "wasm"))))]
use vm::{compile_source, compile_source_for_repl};

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
use std::path::PathBuf;
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
use vm::{compile_source_file, compile_source_for_repl};

const UNRELATED_STANDARD_STRUCTS: [&str; 2] = ["JitConfig", "SqliteLimits"];
const HTTP_NAMED_STRUCTS: [&str; 5] = [
    "HttpRequest",
    "HttpResponse",
    "SseRequest",
    "SseCallbackAction",
    "SseSummary",
];

struct CatalogFreeDialect;
impl ParserDialect for CatalogFreeDialect {}
static CATALOG_FREE_DIALECT: CatalogFreeDialect = CatalogFreeDialect;

fn catalog_free_options() -> SharedParserOptions {
    SharedParserOptions {
        source_id: 0,
        allow_implicit_externs: false,
        allow_implicit_semicolons: false,
        enforce_mutable_bindings: true,
        import_scan_mode: false,
    }
}

fn catalog_free_parse(source: &str) -> Result<FrontendIr, ParseError> {
    parse_source_with_dialect(source, &CATALOG_FREE_DIALECT, catalog_free_options())
}

fn catalog_free_import_scan(source: &str) -> Result<FrontendIr, ParseError> {
    parse_source_with_dialect(
        source,
        &CATALOG_FREE_DIALECT,
        SharedParserOptions {
            import_scan_mode: true,
            allow_implicit_externs: true,
            ..catalog_free_options()
        },
    )
}

fn assert_unrelated_standard_structs_absent(ir: &FrontendIr) {
    assert!(
        ir.host_api_metadata.is_none(),
        "catalog-free parse must not attach host catalog metadata"
    );
    for name in UNRELATED_STANDARD_STRUCTS {
        assert!(
            !ir.struct_schemas.contains_key(name),
            "catalog-free parse must not reserve {name} without host functions"
        );
    }
}

fn widget_catalog() -> Arc<HostApiCatalog> {
    let widget = HostStructSchema::new(
        "Widget",
        vec![HostStructField::new("label", HostTypeSchema::String)],
    );
    let mut builder = HostApiBuilder::new();
    builder.named_struct(widget.clone());
    builder.function(HostFunctionSchema::with_return(
        "widget::origin",
        vec![],
        widget.as_type(),
    ));
    Arc::new(builder.build().expect("widget catalog must build"))
}

/// Panic-safe unique `.rss` file under `std::env::temp_dir()`.
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
struct TempRssPath {
    path: PathBuf,
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
impl TempRssPath {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "{name}_{}_{}.rss",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        ));
        Self { path }
    }
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
impl Drop for TempRssPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn catalog_free_dialect_parse_does_not_reserve_unrelated_standard_structs() {
    let ir = catalog_free_parse("1;").expect("trivial source must parse");
    assert_unrelated_standard_structs_absent(&ir);
}

#[test]
fn catalog_free_import_scan_does_not_reserve_unrelated_standard_structs() {
    let ir = catalog_free_import_scan("use widget;\n1;\n").expect("import scan must parse");
    assert_unrelated_standard_structs_absent(&ir);
}

#[test]
fn catalog_free_parse_rejects_jit_config_type() {
    let error = catalog_free_parse("fn go() -> JitConfig { 1 }")
        .expect_err("JitConfig must be unknown without a catalog");
    assert!(
        error.message.contains("unknown struct schema 'JitConfig'"),
        "catalog-free parse must reject JitConfig as unknown, got {}",
        error.message
    );
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn catalog_free_http_parse_installs_sse_named_structs() {
    let ir = catalog_free_parse("1;").expect("trivial source must parse");
    assert_unrelated_standard_structs_absent(&ir);
    for name in HTTP_NAMED_STRUCTS {
        assert!(
            ir.struct_schemas.contains_key(name),
            "catalog-free HTTP parse must install {name}"
        );
    }
    catalog_free_parse("fn go() -> SseCallbackAction { { action: \"continue\" } }")
        .expect("SseCallbackAction must parse on the catalog-free HTTP path");
}

#[cfg(not(all(feature = "http-client", not(target_family = "wasm"))))]
#[test]
fn catalog_free_without_http_installs_no_fallback_structs() {
    let ir = catalog_free_parse("1;").expect("trivial source must parse");
    assert_unrelated_standard_structs_absent(&ir);
    for name in HTTP_NAMED_STRUCTS {
        assert!(
            !ir.struct_schemas.contains_key(name),
            "catalog-free parse without HTTP must not install {name}"
        );
    }
    let error = catalog_free_parse("fn go() -> SseCallbackAction { { action: \"continue\" } }")
        .expect_err("SSE structs must be unknown without HTTP");
    assert!(
        error
            .message
            .contains("unknown struct schema 'SseCallbackAction'"),
        "without HTTP, SseCallbackAction must stay unknown, got {}",
        error.message
    );
}

#[test]
fn custom_catalog_remains_authoritative() {
    let catalog = widget_catalog();
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use widget;
        fn go() -> Widget { { label: "ok" } }
        widget::origin();
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("custom catalog named structs must compile");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "widget::origin"),
        "custom catalog host functions must remain visible"
    );
    assert!(
        !compiled.program.named_struct_decls().contains_key("Widget"),
        "custom catalog structs must stay registry-side, not on the guest VMBC table"
    );

    let sse_message = match compile_source_with_flavor_and_options(
        r#"fn go() -> SseCallbackAction { { action: "continue" } }"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    ) {
        Ok(_) => panic!("custom catalog must not inherit HTTP fallback structs"),
        Err(err) => err.to_string(),
    };
    assert!(
        sse_message.contains("unknown struct schema 'SseCallbackAction'"),
        "custom catalog must keep SseCallbackAction unknown, got {sse_message}"
    );

    let jit_message = match compile_source_with_flavor_and_options(
        "fn go() -> JitConfig { 1 }",
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    ) {
        Ok(_) => panic!("custom catalog must not inherit JitConfig"),
        Err(err) => err.to_string(),
    };
    assert!(
        jit_message.contains("unknown struct schema 'JitConfig'"),
        "custom catalog must keep JitConfig unknown, got {jit_message}"
    );
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn compile_source_file_without_catalog_admits_sse_named_structs() {
    let temp = TempRssPath::new("pd_vm_catalog_free_sse_file");
    std::fs::write(
        &temp.path,
        "fn go() -> SseCallbackAction { { action: \"continue\" } }\n",
    )
    .expect("temp rss must write");
    compile_source_file(&temp.path).unwrap_or_else(|err| {
        panic!("file frontend without an explicit catalog must admit SseCallbackAction, got {err}")
    });
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn compile_source_for_repl_without_catalog_admits_sse_named_structs() {
    compile_source_for_repl("fn go() -> SseCallbackAction { { action: \"continue\" } }")
        .unwrap_or_else(|err| {
            panic!("REPL without an explicit catalog must admit SseCallbackAction, got {err}")
        });
}

#[cfg(not(all(feature = "http-client", not(target_family = "wasm"))))]
#[test]
fn compile_without_http_does_not_reserve_jit_config() {
    let compile_message = match compile_source("fn go() -> JitConfig { 1 }") {
        Ok(_) => panic!("JitConfig must not compile without a catalog"),
        Err(err) => err.to_string(),
    };
    assert!(
        compile_message.contains("unknown struct schema 'JitConfig'"),
        "no-http compile must not reserve JitConfig, got {compile_message}"
    );

    let repl_message = match compile_source_for_repl("fn go() -> JitConfig { 1 }") {
        Ok(_) => panic!("REPL without HTTP must not reserve JitConfig"),
        Err(err) => err.to_string(),
    };
    assert!(
        repl_message.contains("unknown struct schema 'JitConfig'"),
        "no-http REPL must not reserve JitConfig, got {repl_message}"
    );
}
