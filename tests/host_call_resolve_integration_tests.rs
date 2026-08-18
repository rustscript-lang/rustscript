//! Integration coverage for the compiler-owned
//! [`vm::HostCallResolver`] call-resolution adapter.
//!
//! These tests drive the adapter through the public crate-root API exactly as
//! parser/compiler catalog integration will: build a concrete `io.file` /
//! `sqlite.connection` catalog, then resolve host calls from a function name
//! plus actual argument [`TypeSchema`] values. They focus on nominal resource
//! overloads, return inference, ownership-passing preservation, `Unknown`
//! ambiguity/fallback, diagnostics, nested resource schemas and fingerprint
//! propagation.

use vm::compiler::TypeSchema;
use vm::{
    HostApiBuilder, HostApiCatalog, HostCallResolveError, HostCallResolver, HostFunctionSchema,
    HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};

fn io_file() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("valid key")
}

fn sqlite_conn() -> ResourceTypeKey {
    ResourceTypeKey::new("sqlite.connection").expect("valid key")
}

fn resource(key: ResourceTypeKey) -> HostTypeSchema {
    HostTypeSchema::Resource(key)
}

fn res(key: ResourceTypeKey) -> TypeSchema {
    TypeSchema::Resource(key)
}

fn value(name: &str, ty: HostTypeSchema) -> HostParamSchema {
    HostParamSchema::value(name, ty)
}

/// The canonical concrete catalog: two distinct nominal resource types, plus a
/// resource-typed overload (`forward`), ownership-mode variants, a nested
/// (container) resource schema, and a scalar overload set.
fn concrete_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(io_file(), "An open file"));
    builder.resource(ResourceTypeSchema::new(
        sqlite_conn(),
        "An open SQLite connection",
    ));

    builder.function(HostFunctionSchema::with_return(
        "io::open",
        vec![value("path", HostTypeSchema::String)],
        resource(io_file()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::open",
        vec![value("path", HostTypeSchema::String)],
        resource(sqlite_conn()),
    ));

    // Overloaded by resource type: identical name and arity, differing only in
    // the nominal resource key, so dispatch is on resource identity.
    builder.function(HostFunctionSchema::with_return(
        "forward",
        vec![HostParamSchema::with_passing(
            "h",
            resource(io_file()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "forward",
        vec![HostParamSchema::with_passing(
            "h",
            resource(sqlite_conn()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::String,
    ));

    // Distinct ownership modes preserved across resolution.
    builder.function(HostFunctionSchema::with_return(
        "file::read",
        vec![HostParamSchema::with_passing(
            "handle",
            resource(io_file()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
    ));
    builder.function(HostFunctionSchema::with_return(
        "file::mutate",
        vec![HostParamSchema::with_passing(
            "handle",
            resource(io_file()),
            HostParamPassing::BorrowMut,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "file::reap",
        vec![HostParamSchema::with_passing(
            "handle",
            resource(io_file()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));

    // Nested (container) resource schema.
    builder.function(HostFunctionSchema::with_return(
        "collect",
        vec![HostParamSchema::with_passing(
            "files",
            HostTypeSchema::Array(Box::new(resource(io_file()))),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));

    // Scalar overload set for Unknown ambiguity / fallback checks.
    builder.function(HostFunctionSchema::with_return(
        "parse",
        vec![value("v", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "parse",
        vec![value("v", HostTypeSchema::String)],
        HostTypeSchema::String,
    ));

    builder.build().expect("concrete catalog must build")
}

#[test]
fn overload_by_resource_type_uses_nominal_key() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    // Same name `forward`, same arity, differing resource key => dispatch on
    // the nominal identity and return inference follows the overload.
    let file = resolver
        .resolve("forward", &[res(io_file())])
        .expect("io.file overload");
    assert_eq!(file.return_type, TypeSchema::Int);
    assert_eq!(file.passing, vec![HostParamPassing::TakeOwned]);

    let db = resolver
        .resolve("forward", &[res(sqlite_conn())])
        .expect("sqlite overload");
    assert_eq!(db.return_type, TypeSchema::String);
}

#[test]
fn correct_return_inference_across_resource_returns() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    let io = resolver
        .resolve("io::open", &[TypeSchema::String])
        .expect("io::open resolves");
    assert_eq!(io.return_type, res(io_file()));

    let sqlite = resolver
        .resolve("sqlite::open", &[TypeSchema::String])
        .expect("sqlite::open resolves");
    assert_eq!(sqlite.return_type, res(sqlite_conn()));

    // Source -> compiler return mapping keeps the nominal resource identity.
    assert_eq!(io.return_type, TypeSchema::Resource(io_file()));
}

#[test]
fn borrow_take_ownership_modes_are_preserved() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    assert_eq!(
        resolver
            .resolve("file::read", &[res(io_file())])
            .expect("read resolves")
            .passing,
        vec![HostParamPassing::Borrow]
    );
    assert_eq!(
        resolver
            .resolve("file::mutate", &[res(io_file())])
            .expect("mutate resolves")
            .passing,
        vec![HostParamPassing::BorrowMut]
    );
    assert_eq!(
        resolver
            .resolve("file::reap", &[res(io_file())])
            .expect("reap resolves")
            .passing,
        vec![HostParamPassing::TakeOwned]
    );
}

#[test]
fn wrong_resource_is_a_concrete_mismatch_never_a_structural_fallback() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    // file::read expects resource<io.file>; a sqlite connection must not match.
    let err = resolver
        .resolve("file::read", &[res(sqlite_conn())])
        .expect_err("io.file and sqlite.connection are nominal, not interchangeable");
    match err {
        HostCallResolveError::NoMatch { name, detail } => {
            assert_eq!(name, "file::read");
            assert!(
                detail.contains("expected resource<io.file>"),
                "missing expected diagnostic: {detail}"
            );
            assert!(
                detail.contains("found resource<sqlite.connection>"),
                "missing found diagnostic: {detail}"
            );
        }
        other => panic!("expected NoMatch, got {other:?}"),
    }
}

#[test]
fn unknown_argument_is_deferred_but_ties_are_ambiguous() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    // Several scalar overloads share the name; a totally unknown argument makes
    // them equally viable => structured ambiguity, no silent pick.
    assert!(matches!(
        resolver.resolve("parse", &[TypeSchema::Unknown]),
        Err(HostCallResolveError::Ambiguous { name, .. }) if name == "parse"
    ));
    // A concrete argument breaks the tie deterministically.
    let resolved = resolver
        .resolve("parse", &[TypeSchema::Int])
        .expect("Int resolves the Int overload");
    assert_eq!(resolved.params[0].schema, TypeSchema::Int);
}

#[test]
fn arity_mismatch_is_distinct_from_type_mismatch() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    assert!(matches!(
        resolver.resolve("file::read", &[res(io_file()), TypeSchema::String]),
        Err(HostCallResolveError::ArityMismatch { .. })
    ));
}

#[test]
fn unknown_function_is_a_distinct_error() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    assert!(matches!(
        resolver.resolve("does_not_exist", &[]),
        Err(HostCallResolveError::UnknownFunction(name)) if name == "does_not_exist"
    ));
}

#[test]
fn nested_resource_schema_resolves_and_stays_nominal() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    let resolved = resolver
        .resolve("collect", &[TypeSchema::Array(Box::new(res(io_file())))])
        .expect("nested io.file array resolves");
    assert_eq!(
        resolved.params[0].schema,
        TypeSchema::Array(Box::new(res(io_file())))
    );
    // io.file and sqlite.connection do not unify inside a container.
    assert!(matches!(
        resolver.resolve(
            "collect",
            &[TypeSchema::Array(Box::new(res(sqlite_conn())))]
        ),
        Err(HostCallResolveError::NoMatch { .. })
    ));
}

#[test]
fn fingerprint_propagates_into_resolved_result() {
    let catalog = concrete_catalog();
    let resolver = HostCallResolver::new(&catalog);
    let resolved = resolver
        .resolve("file::read", &[res(io_file())])
        .expect("resolves");
    assert_eq!(resolved.fingerprint, resolver.fingerprint());
    assert_eq!(resolved.fingerprint, catalog.fingerprint());
}
