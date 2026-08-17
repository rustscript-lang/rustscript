//! Integration coverage for the shared host-agnostic [`vm::host_api`] catalog.
//!
//! These tests exercise the public API surface exactly as consumers (compiler,
//! VM binding, language tooling) would: build concrete `io.file` and
//! `sqlite.connection` catalogs, validate, and fingerprint.

use vm::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};

fn io_file() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("valid key")
}

fn sqlite_connection() -> ResourceTypeKey {
    ResourceTypeKey::new("sqlite.connection").expect("valid key")
}

/// The canonical concrete catalog used across these tests.
fn concrete_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(io_file(), "An open file handle"));
    builder.resource(ResourceTypeSchema::new(
        sqlite_connection(),
        "An open SQLite database connection",
    ));

    builder.function(
        HostFunctionSchema::with_return(
            "io::open",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("mode", HostTypeSchema::String),
            ],
            HostTypeSchema::Resource(io_file()),
        )
        .with_description("Open a file handle."),
    );
    builder.function(HostFunctionSchema::new(
        "io::read_all",
        vec![HostParamSchema::value(
            "handle",
            HostTypeSchema::Resource(io_file()),
        )],
    ));
    builder.function(HostFunctionSchema::new(
        "sqlite::open",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
    ));
    builder.function(HostFunctionSchema::new(
        "sqlite::exec",
        vec![
            HostParamSchema::with_passing(
                "db",
                HostTypeSchema::Resource(sqlite_connection()),
                HostParamPassing::BorrowMut,
            ),
            HostParamSchema::value("sql", HostTypeSchema::String),
        ],
    ));

    builder.build().expect("concrete catalog must build")
}

#[test]
fn concrete_io_file_schema() {
    let catalog = concrete_catalog();
    let open = catalog.function("io::open").expect("io::open");
    assert_eq!(open.return_type, HostTypeSchema::Resource(io_file()));
    assert_eq!(open.params.len(), 2);
    assert!(catalog.resource("io.file").is_some());
}

#[test]
fn concrete_sqlite_connection_schema() {
    let catalog = concrete_catalog();
    let exec = catalog.function("sqlite::exec").expect("sqlite::exec");
    assert_eq!(exec.params[0].passing, HostParamPassing::BorrowMut);
    assert_eq!(
        exec.params[0].ty,
        HostTypeSchema::Resource(sqlite_connection())
    );
    assert!(catalog.resource("sqlite.connection").is_some());
}

#[test]
fn fingerprints_are_order_independent_from_integration() {
    let a = concrete_catalog();
    let b = reordered_catalog();
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_ne!(a.fingerprint().as_u64(), 0);
}

/// The same semantic content as [`concrete_catalog`] but registered in a
/// different order, so a fingerprint comparison proves order-independence.
fn reordered_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::new(
        "sqlite::exec",
        vec![
            HostParamSchema::with_passing(
                "db",
                HostTypeSchema::Resource(sqlite_connection()),
                HostParamPassing::BorrowMut,
            ),
            HostParamSchema::value("sql", HostTypeSchema::String),
        ],
    ));
    builder.resource(ResourceTypeSchema::new(sqlite_connection(), "db"));
    builder.function(HostFunctionSchema::new(
        "io::read_all",
        vec![HostParamSchema::value(
            "handle",
            HostTypeSchema::Resource(io_file()),
        )],
    ));
    builder.resource(ResourceTypeSchema::new(io_file(), "file"));
    builder.function(HostFunctionSchema::with_return(
        "io::open",
        vec![
            HostParamSchema::value("path", HostTypeSchema::String),
            HostParamSchema::value("mode", HostTypeSchema::String),
        ],
        HostTypeSchema::Resource(io_file()),
    ));
    builder.function(HostFunctionSchema::new(
        "sqlite::open",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
    ));
    builder.build().expect("reordered catalog must build")
}

#[test]
fn public_api_reachable_without_runtime_feature() {
    // Reachability smoke test: the host-api types are exported from the crate
    // root and do not depend on the `runtime` feature.
    let key = io_file();
    let schema = HostTypeSchema::Resource(key);
    let param = HostParamSchema::value("x", HostTypeSchema::Int);
    let function = HostFunctionSchema::new("f", vec![param]);
    assert_eq!(function.name, "f");
    assert_eq!(
        schema.resource_key().map(ResourceTypeKey::as_str),
        Some("io.file")
    );
    assert!(!HostParamPassing::Value.is_reference_mode());
}
