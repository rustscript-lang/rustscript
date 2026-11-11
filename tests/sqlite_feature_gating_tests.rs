//! Feature-disabled and feature-enabled admission of the standard SQLite
//! surface.
//!
//! The native SQLite catalog is optional: a build without `sqlite` must not
//! publish any `sqlite::*` function or `Sqlite*` named struct, while a build
//! with the feature publishes the complete typed surface. This target is
//! deliberately separate from the behavioral SQLite tags so its only job is
//! the real `standard_host_catalog` admission boundary.

#![cfg(feature = "runtime")]

use vm::standard_host_catalog;

#[cfg(not(feature = "sqlite"))]
#[test]
fn standard_catalog_omits_sqlite_surface_without_the_feature() {
    let catalog = standard_host_catalog();
    assert!(
        catalog
            .functions()
            .iter()
            .all(|function| !function.name.starts_with("sqlite::")),
        "a non-sqlite build must publish no sqlite host function"
    );
    assert!(
        catalog
            .structs()
            .iter()
            .all(|schema| !schema.name.starts_with("Sqlite")),
        "a non-sqlite build must publish no Sqlite named struct"
    );
    assert!(
        catalog
            .resources()
            .iter()
            .all(|resource| resource.key.as_str() != "sqlite.connection"),
        "a non-sqlite build must publish no sqlite.connection resource"
    );
}

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
#[test]
fn standard_catalog_publishes_the_typed_sqlite_surface_with_the_feature() {
    let catalog = standard_host_catalog();
    for name in [
        "sqlite::open",
        "sqlite::execute",
        "sqlite::query",
        "sqlite::transaction",
        "sqlite::close",
    ] {
        assert!(
            catalog.function(name).is_some(),
            "an sqlite build must publish `{name}`"
        );
    }
    assert_eq!(
        catalog
            .function("sqlite::close")
            .expect("sqlite::close")
            .return_type,
        vm::HostTypeSchema::Unit,
        "sqlite::close exposes logical Unit"
    );
    for name in [
        "SqliteOpenOptions",
        "SqliteLimits",
        "SqliteExecuteResult",
        "SqliteValue",
        "SqliteRow",
        "SqliteQueryResult",
        "SqliteStatement",
        "SqliteTransactionResult",
    ] {
        assert!(
            catalog.struct_named(name).is_some(),
            "an sqlite build must publish `{name}`"
        );
    }
    assert!(
        catalog
            .resources()
            .iter()
            .any(|resource| resource.key.as_str() == "sqlite.connection"),
        "an sqlite build must publish the sqlite.connection resource"
    );
}
