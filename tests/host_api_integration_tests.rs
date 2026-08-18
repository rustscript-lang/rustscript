//! Integration coverage for the shared host-agnostic [`vm::host_api`] catalog.
//!
//! These tests exercise the public API surface exactly as consumers (compiler,
//! VM binding, language tooling) would: build concrete `io.file` and
//! `sqlite.connection` catalogs, validate, and fingerprint.

use vm::{
    FunctionNameError, HostApiBuilder, HostApiCatalog, HostApiCatalogError, HostFunctionSchema,
    HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeKeyError,
    ResourceTypeSchema,
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
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file()),
            HostParamPassing::Borrow,
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
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file()),
            HostParamPassing::Borrow,
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

#[test]
fn len_overloads_are_supported_and_order_independent() {
    let len_string = || {
        HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::String)],
            HostTypeSchema::Int,
        )
    };
    let len_array = || {
        HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value(
                "value",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Int)),
            )],
            HostTypeSchema::Int,
        )
    };
    let len_bytes = || {
        HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::Bytes)],
            HostTypeSchema::Int,
        )
    };

    // Registration order must not affect the overloaded fingerprint.
    let mut a = HostApiCatalog::builder();
    a.function(len_string());
    a.function(len_array());
    a.function(len_bytes());
    let catalog_a = a.build().expect("legal overloads");

    let mut b = HostApiCatalog::builder();
    b.function(len_bytes());
    b.function(len_string());
    b.function(len_array());
    let catalog_b = b.build().expect("legal overloads");

    assert_eq!(catalog_a.fingerprint(), catalog_b.fingerprint());
    assert_eq!(catalog_a.functions_named("len").len(), 3);
    assert!(
        catalog_a.function("len").is_none(),
        "overloaded name is ambiguous"
    );

    // An exact duplicate overload must be rejected.
    let mut c = HostApiCatalog::builder();
    c.function(len_string());
    c.function(len_string());
    assert!(
        c.build().is_err(),
        "exact duplicate overload must be rejected"
    );
}

#[test]
fn host_api_serde_rejects_hostile_json() {
    // Value-passing a containing resource must be rejected by the validating
    // deserializer reached through the public API.
    let hostile = r#"{
        "resources": [{ "key": "io.file", "description": "file" }],
        "functions": [{
            "name": "io::read_all",
            "params": [{ "name": "handle", "ty": { "Resource": "io.file" }, "passing": "Value" }],
            "return_type": "String",
            "description": ""
        }]
    }"#;
    let result: Result<HostApiCatalog, _> = serde_json::from_str(hostile);
    assert!(result.is_err(), "Value-passing a resource must be rejected");
}

#[test]
fn resource_type_key_empty_segment_offset_is_precise() {
    // `a..b` has its empty segment (the doubled dot) at byte offset 2, not at
    // the first dot in the name (byte 1).
    assert_eq!(
        ResourceTypeKey::new("a..b"),
        Err(ResourceTypeKeyError::InvalidDotPlacement { index: 2 })
    );
    // A trailing dot leaves an empty segment at the end-of-name offset.
    assert_eq!(
        ResourceTypeKey::new("a."),
        Err(ResourceTypeKeyError::InvalidDotPlacement { index: 2 })
    );
    // A leading dot starts an empty segment at byte offset 0.
    assert_eq!(
        ResourceTypeKey::new(".a"),
        Err(ResourceTypeKeyError::InvalidDotPlacement { index: 0 })
    );
    // Single-segment keys and segment charset are legal (verified values).
    for legal in ["file", "0host", "a-b_c", "io.file", "sqlite.connection"] {
        assert!(
            ResourceTypeKey::new(legal).is_ok(),
            "`{legal}` must be valid"
        );
    }
}

#[test]
fn function_name_empty_segment_offset_is_precise() {
    // `a::::b` contains an empty `::`-segment that begins at byte 3 (the second
    // `::` group), not at the first `::` at byte 1.
    let mut b = HostApiCatalog::builder();
    b.function(HostFunctionSchema::new("a::::b", vec![]));
    match b.build() {
        Err(HostApiCatalogError::InvalidFunctionName { reason, .. }) => {
            assert_eq!(reason, FunctionNameError::EmptySegment { index: 3 })
        }
        other => panic!("expected InvalidFunctionName, got {other:?}"),
    }

    // A trailing `::` reports the empty final segment at the end-of-name offset.
    let mut b = HostApiCatalog::builder();
    b.function(HostFunctionSchema::new("a::b::", vec![]));
    match b.build() {
        Err(HostApiCatalogError::InvalidFunctionName { reason, .. }) => {
            assert_eq!(reason, FunctionNameError::EmptySegment { index: 6 })
        }
        other => panic!("expected InvalidFunctionName, got {other:?}"),
    }

    // A leading `::` reports the empty first segment at byte offset 0.
    let mut b = HostApiCatalog::builder();
    b.function(HostFunctionSchema::new("::b", vec![]));
    match b.build() {
        Err(HostApiCatalogError::InvalidFunctionName { reason, .. }) => {
            assert_eq!(reason, FunctionNameError::EmptySegment { index: 0 })
        }
        other => panic!("expected InvalidFunctionName, got {other:?}"),
    }
}
