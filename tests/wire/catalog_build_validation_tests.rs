//! Build-time catalog validation tests.
//!
//! `build.rs` is the generator that fails the build when the catalog violates
//! the static builtin ID contract. This suite includes the real build script
//! as a module (the same pattern as `tests/host_binding_generation_tests.rs`)
//! and exercises its catalog parser and contract validator directly:
//! duplicate IDs/names/variants, malformed entries, unsupported feature
//! gates, unknown classes, out-of-block IDs, class/name inconsistencies, and
//! missing/extra runtime callables all must fail validation.

#[allow(dead_code)]
#[path = "../../build.rs"]
mod build_script;

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use build_script::{
    CatalogClass, CatalogEntry, ORDINARY_BLOCK_START, SPECIAL_CALL_BLOCK_END,
    SPECIAL_CALL_BLOCK_START, SQLITE_RESERVED_TOP_END, SQLITE_RESERVED_TOP_START,
    builtin_variant_name, parse_catalog_source, validate_catalog_contract,
};

fn assert_panics<F, R>(f: F)
where
    F: FnOnce() -> R,
{
    let result = catch_unwind(AssertUnwindSafe(f));
    assert!(result.is_err(), "expected a build.rs validation panic");
}

fn catalog_line(id: u16, name: &str, variant: &str, class: &str, gate: &str) -> String {
    format!("builtin_id!(0x{id:04X}, {name:?}, {variant}, {class}, {gate});")
}

fn entry(id: u16, source_name: &str, class: CatalogClass) -> CatalogEntry {
    CatalogEntry {
        id,
        source_name: source_name.to_string(),
        variant: builtin_variant_name(source_name),
        class,
        feature_gate: "none".to_string(),
    }
}

fn names(values: &[&str]) -> HashSet<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn parse_catalog_source_accepts_the_checked_in_catalog() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/builtins/catalog.rs"
    ))
    .expect("read authoritative catalog");
    let entries = parse_catalog_source(&source, "catalog.rs");
    // The SQLite namespace is optional (mirrors the build.rs feature filter):
    // when the feature is off, the generated catalog excludes it.
    #[cfg(not(feature = "sqlite"))]
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| !entry.source_name.starts_with("sqlite::"))
        .collect();
    assert!(!entries.is_empty());
    assert_eq!(entries.len(), vm::BUILTIN_CATALOG.len());
}

#[test]
fn parse_catalog_source_rejects_duplicate_ids() {
    let source = format!(
        "{}\n{}",
        catalog_line(0xFFA2, "len", "Len", "Ordinary", "none"),
        catalog_line(0xFFA2, "get", "Get", "Ordinary", "none"),
    );
    assert_panics(|| parse_catalog_source(&source, "test"));
}

#[test]
fn parse_catalog_source_rejects_duplicate_source_names() {
    let source = format!(
        "{}\n{}",
        catalog_line(0xFFA2, "len", "Len", "Ordinary", "none"),
        catalog_line(0xFFA3, "len", "LenAlias", "Ordinary", "none"),
    );
    assert_panics(|| parse_catalog_source(&source, "test"));
}

#[test]
fn parse_catalog_source_rejects_duplicate_variants() {
    let source = format!(
        "{}\n{}",
        catalog_line(0xFFA2, "len", "Len", "Ordinary", "none"),
        catalog_line(0xFFA3, "len_alias", "Len", "Ordinary", "none"),
    );
    assert_panics(|| parse_catalog_source(&source, "test"));
}

#[test]
fn parse_catalog_source_rejects_unsupported_feature_gates() {
    let source = catalog_line(0xFFA2, "len", "Len", "Ordinary", "sqlite");
    assert_panics(|| parse_catalog_source(&source, "test"));
}

#[test]
fn parse_catalog_source_rejects_unknown_classes() {
    let source = catalog_line(0xFFA2, "len", "Len", "Host", "none");
    assert_panics(|| parse_catalog_source(&source, "test"));
}

#[test]
fn parse_catalog_source_rejects_malformed_entries() {
    // Wrong field count.
    assert_panics(|| parse_catalog_source("builtin_id!(0xFFA2, \"len\", Len, Ordinary);", "test"));
    // Non-hex id.
    assert_panics(|| {
        parse_catalog_source("builtin_id!(0xZZZZ, \"len\", Len, Ordinary, none);", "test")
    });
    // Unquoted source name.
    assert_panics(|| {
        parse_catalog_source("builtin_id!(0xFFA2, len, Len, Ordinary, none);", "test")
    });
    // Missing terminator.
    assert_panics(|| {
        parse_catalog_source("builtin_id!(0xFFA2, \"len\", Len, Ordinary, none)", "test")
    });
    // A line that is neither a comment nor an entry.
    assert_panics(|| parse_catalog_source("fn main() {}", "test"));
}

#[test]
fn validate_catalog_contract_accepts_a_valid_catalog() {
    let entries = vec![
        entry(0xFFA2, "len", CatalogClass::Ordinary),
        entry(0xFF93, "__detach_local", CatalogClass::Internal),
        entry(0xFFA0, "type", CatalogClass::Special),
    ];
    let discovered = names(&["len", "__detach_local", "type"]);
    let special = names(&["DetachLocal", "TypeOf"]);
    validate_catalog_contract(&entries, &discovered, &special);
}

#[test]
fn validate_catalog_contract_rejects_arithmetic_allocation_in_sqlite_top_range() {
    for id in SQLITE_RESERVED_TOP_START..=SQLITE_RESERVED_TOP_END {
        let entries = vec![entry(id, "len", CatalogClass::Ordinary)];
        let discovered = names(&["len"]);
        let special = HashSet::new();
        assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
    }
}

#[test]
fn validate_catalog_contract_rejects_out_of_block_ordinary_ids() {
    for bad_id in [
        SPECIAL_CALL_BLOCK_START - 1, // extension block
        SPECIAL_CALL_BLOCK_END,       // special-call block
    ] {
        let entries = vec![entry(bad_id, "len", CatalogClass::Ordinary)];
        let discovered = names(&["len"]);
        let special = HashSet::new();
        assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
    }
}

#[test]
fn validate_catalog_contract_rejects_out_of_block_special_ids() {
    let entries = vec![entry(ORDINARY_BLOCK_START, "type", CatalogClass::Special)];
    let discovered = names(&["type"]);
    let special = names(&["TypeOf"]);
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_internal_class_without_internal_name() {
    let entries = vec![entry(
        SPECIAL_CALL_BLOCK_START,
        "type",
        CatalogClass::Internal,
    )];
    let discovered = names(&["type"]);
    let special = names(&["TypeOf"]);
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_special_class_with_internal_name() {
    let entries = vec![entry(
        SPECIAL_CALL_BLOCK_START,
        "__detach_local",
        CatalogClass::Special,
    )];
    let discovered = names(&["__detach_local"]);
    let special = names(&["DetachLocal"]);
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_variant_mismatches() {
    let mut entries = vec![entry(0xFFA2, "len", CatalogClass::Ordinary)];
    entries[0].variant = "WrongVariant".to_string();
    let discovered = names(&["len"]);
    let special = HashSet::new();
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_missing_callables() {
    // A runtime callable without an explicit catalog ID.
    let entries = vec![entry(0xFFA2, "len", CatalogClass::Ordinary)];
    let discovered = names(&["len", "unlisted_builtin"]);
    let special = HashSet::new();
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_entries_without_callables() {
    // A catalog entry with no runtime callable (typo).
    let entries = vec![entry(0xFFA2, "len", CatalogClass::Ordinary)];
    let discovered = names(&[]);
    let special = HashSet::new();
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_ordinary_class_for_special_dispatch() {
    let entries = vec![entry(0xFFA2, "len", CatalogClass::Ordinary)];
    let discovered = names(&["len"]);
    let special = names(&["Len"]);
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}

#[test]
fn validate_catalog_contract_rejects_special_class_for_ordinary_dispatch() {
    let entries = vec![entry(
        SPECIAL_CALL_BLOCK_START,
        "type",
        CatalogClass::Special,
    )];
    let discovered = names(&["type"]);
    // "type" is a language builtin, so it dispatches through the special path;
    // force the opposite (not special) to prove the class/dispatch check.
    let special = names(&["Unrelated"]);
    assert_panics(|| validate_catalog_contract(&entries, &discovered, &special));
}
