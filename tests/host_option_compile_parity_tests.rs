#![cfg(all(feature = "runtime", feature = "sqlite"))]

use vm::{
    CompileSourceFileOptions, SourceFlavor, analyze_source_from_string_with_options,
    compile_source_with_flavor_and_options, standard_host_catalog,
};

const OMITTED: &str = r#"use sqlite;
fn main() {
    let db = sqlite::open({});
    sqlite::query(&db, "SELECT 1", [], {});
    true;
}
"#;

const EXPLICIT: &str = r#"use sqlite;
fn main() {
    let db = sqlite::open({
        path: Option::Some(""),
        mode: Option::Some(""),
        root: Option::Some(""),
        limits: Option::Some({ max_rows: Option::Some(0), busy_timeout_ms: Option::Some(0) })
    });
    sqlite::query(&db, "SELECT 1", [], { max_rows: Option::Some(0), busy_timeout_ms: Option::Some(0) });
    let statement: SqliteStatement = {
        sql: "SELECT 1",
        query: Option::Some(false),
        params: Option::None,
        limits: Option::None
    };
    statement.sql == "SELECT 1";
}
"#;

const WRONG_TYPE: &str = r#"use sqlite;
fn main() {
    sqlite::open({ path: Option::Some(false) });
    true;
}
"#;

fn catalog_options() -> CompileSourceFileOptions {
    CompileSourceFileOptions::new().with_host_api_catalog(standard_host_catalog())
}

fn analyze_messages(source: &str) -> Vec<String> {
    let model = analyze_source_from_string_with_options("parity.rss", source, catalog_options())
        .expect("configured catalog analysis should parse");
    model
        .diagnostics()
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn compile_message(source: &str) -> Result<(), String> {
    compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, catalog_options())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[test]
fn sqlite_omitted_and_explicit_options_match_across_compile_and_analyze() {
    let omitted_diagnostics = analyze_messages(OMITTED);
    assert!(
        omitted_diagnostics.is_empty(),
        "analyze must accept omitted sqlite options: {omitted_diagnostics:?}"
    );
    compile_message(OMITTED).expect("compile must accept the same omitted sqlite options");

    let explicit_diagnostics = analyze_messages(EXPLICIT);
    assert!(
        explicit_diagnostics.is_empty(),
        "analyze must accept Some(0)/Some(empty)/Some(false): {explicit_diagnostics:?}"
    );
    compile_message(EXPLICIT)
        .expect("compile must accept the same explicit Some(0)/Some(empty)/Some(false)");
}

#[test]
fn sqlite_wrong_option_type_fails_on_compile_and_analyze() {
    let analyzed = analyze_messages(WRONG_TYPE);
    assert!(
        !analyzed.is_empty(),
        "analyze must reject Some(false) for a string option"
    );
    let compiled =
        compile_message(WRONG_TYPE).expect_err("compile must reject the same wrong type");
    assert!(
        compiled.contains("string") || compiled.contains("Option") || compiled.contains("path"),
        "compile rejection should name the option type, got {compiled}"
    );
}

#[test]
fn sqlite_close_takes_owned_connection_and_query_borrows() {
    let owned = r#"use sqlite;
fn main() {
    let db = sqlite::open({});
    sqlite::query(&db, "SELECT 1", [], {});
    sqlite::close(db);
    true;
}
"#;
    assert!(
        analyze_messages(owned).is_empty(),
        "owned close should analyze"
    );
    compile_message(owned).expect("owned close should compile");

    let borrowed_close = r#"use sqlite;
fn main() {
    let db = sqlite::open({});
    sqlite::close(&db);
    true;
}
"#;
    assert!(
        !analyze_messages(borrowed_close).is_empty(),
        "close must not accept a borrow"
    );
    assert!(
        compile_message(borrowed_close).is_err(),
        "compile must reject borrowed close"
    );
}
