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

use std::sync::Arc;

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::{
    HostApiBuilder, HostApiCatalog, HostCallResolveError, HostCallResolver, HostFunctionSchema,
    HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
    compile_source_with_flavor_and_options,
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
fn compiler_uses_declared_passing_for_custom_io_namespaces() {
    let key = io_file();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key.clone(), "An open file"));
    builder.function(HostFunctionSchema::with_return(
        "io::open_custom",
        vec![value("path", HostTypeSchema::String)],
        resource(key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "io::take_owned_custom",
        vec![HostParamSchema::with_passing(
            "handle",
            resource(key.clone()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "io::borrow_mut_custom",
        vec![HostParamSchema::with_passing(
            "handle",
            resource(key),
            HostParamPassing::BorrowMut,
        )],
        HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().expect("custom catalog must build"));

    let compiled = compile_source_with_flavor_and_options(
        r#"
        use io;
        let owned = io::open_custom("owned");
        io::take_owned_custom(owned);
        let mut borrowed = io::open_custom("borrowed");
        io::borrow_mut_custom(&mut borrowed);
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("custom IO-prefixed catalog should compile");

    let passing = compiled
        .program
        .imports
        .iter()
        .filter(|import| import.name.ends_with("_custom"))
        .map(|import| {
            (
                import.name.as_str(),
                import
                    .schema
                    .as_ref()
                    .expect("custom import should have exact schema")
                    .params
                    .iter()
                    .map(|param| param.passing)
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    assert!(passing.contains(&("io::take_owned_custom", vec![HostParamPassing::TakeOwned])));
    assert!(passing.contains(&("io::borrow_mut_custom", vec![HostParamPassing::BorrowMut])));
}

#[cfg(feature = "runtime")]
#[test]
fn standard_io_bare_resource_handle_stays_borrowed() {
    let compiled = compile_source_with_flavor_and_options(
        r#"
        use io;
        let handle = io::open("file", "r");
        io::read_all(handle);
        "#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(vm::standard_host_catalog()),
    )
    .expect("standard IO read_all should accept a bare legacy handle");

    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "io::read_all" && import.arity == 1)
        .expect("read_all import");
    assert_eq!(
        import
            .schema
            .as_ref()
            .expect("exact read_all schema")
            .params[0]
            .passing,
        HostParamPassing::Borrow
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

#[test]
fn scalar_int_number_float_selection() {
    // `scale` overloads only on scalar schemas: f(Int) and f(Number).
    // Int resolves the Int overload (exact beats numeric-compat), Number the
    // Number overload, and Float must land on f(Number) because f(Int) is a
    // concrete mismatch for a Float.
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "scale",
        vec![value("v", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "scale",
        vec![value("v", HostTypeSchema::Number)],
        HostTypeSchema::String,
    ));
    let catalog = builder.build().expect("valid scalar overloads");
    let resolver = HostCallResolver::new(&catalog);

    let via_int = resolver
        .resolve("scale", &[TypeSchema::Int])
        .expect("Int resolves");
    assert_eq!(via_int.return_type, TypeSchema::Int);

    let via_number = resolver
        .resolve("scale", &[TypeSchema::Number])
        .expect("Number resolves");
    assert_eq!(via_number.return_type, TypeSchema::String);

    let via_float = resolver
        .resolve("scale", &[TypeSchema::Float])
        .expect("Float resolves");
    assert_eq!(
        via_float.return_type,
        TypeSchema::String,
        "Float must pick f(Number)"
    );
}

#[test]
fn nested_array_numeric_specificity() {
    // array<Int> (exact) must outrank array<Number> (nested numeric-compatible)
    // for an actual array<Int>; array<Number> wins for array<Number>/array<Float>.
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "sum",
        vec![value(
            "xs",
            HostTypeSchema::Array(Box::new(HostTypeSchema::Int)),
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "sum",
        vec![value(
            "xs",
            HostTypeSchema::Array(Box::new(HostTypeSchema::Number)),
        )],
        HostTypeSchema::String,
    ));
    let catalog = builder.build().expect("valid overloads");
    let resolver = HostCallResolver::new(&catalog);

    let ints = resolver
        .resolve("sum", &[TypeSchema::Array(Box::new(TypeSchema::Int))])
        .expect("int array resolves");
    assert_eq!(
        ints.return_type,
        TypeSchema::Int,
        "exact array<Int> must beat numeric array<Number> for an actual array<Int>"
    );

    let floats = resolver
        .resolve("sum", &[TypeSchema::Array(Box::new(TypeSchema::Float))])
        .expect("float array resolves");
    assert_eq!(
        floats.return_type,
        TypeSchema::String,
        "array<Int> is non-viable for array<Float>; array<Number> matches"
    );
}

#[test]
fn reversed_registration_yields_identical_nomatch_and_arity() {
    fn take_catalog(io_first: bool) -> HostApiCatalog {
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(io_file(), "file"));
        builder.resource(ResourceTypeSchema::new(sqlite_conn(), "db"));
        let io = HostFunctionSchema::with_return(
            "take",
            vec![HostParamSchema::with_passing(
                "h",
                resource(io_file()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        );
        let sqlite = HostFunctionSchema::with_return(
            "take",
            vec![HostParamSchema::with_passing(
                "h",
                resource(sqlite_conn()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::String,
        );
        if io_first {
            builder.function(io);
            builder.function(sqlite);
        } else {
            builder.function(sqlite);
            builder.function(io);
        }
        builder.build().expect("valid")
    }

    // NoMatch: a String mismatches both resource overloads; the reported best
    // candidate and detail must be identical regardless of registration order.
    let err_a = HostCallResolver::new(&take_catalog(true))
        .resolve("take", &[TypeSchema::String])
        .unwrap_err();
    let err_b = HostCallResolver::new(&take_catalog(false))
        .resolve("take", &[TypeSchema::String])
        .unwrap_err();
    match (err_a, err_b) {
        (
            HostCallResolveError::NoMatch { detail: a, .. },
            HostCallResolveError::NoMatch { detail: b, .. },
        ) => {
            assert_eq!(a, b, "NoMatch detail must not depend on registration order");
            assert!(a.contains("resource<io.file>"), "surprising detail: {a}");
        }
        (a, b) => panic!("expected NoMatch in both orders, got {a:?} / {b:?}"),
    }
}

#[test]
fn reversed_registration_yields_identical_arity_mismatch_variants() {
    fn g_catalog(forward: bool) -> HostApiCatalog {
        let mut builder = HostApiBuilder::new();
        let one = HostFunctionSchema::with_return(
            "g",
            vec![value("a", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        );
        let two_str = HostFunctionSchema::with_return(
            "g",
            vec![
                value("a", HostTypeSchema::String),
                value("b", HostTypeSchema::String),
            ],
            HostTypeSchema::String,
        );
        if forward {
            builder.function(one);
            builder.function(two_str);
        } else {
            builder.function(two_str);
            builder.function(one);
        }
        builder.build().expect("valid")
    }

    let args = [TypeSchema::Int, TypeSchema::Int, TypeSchema::Int];
    let err_a = HostCallResolver::new(&g_catalog(true))
        .resolve("g", &args)
        .unwrap_err();
    let err_b = HostCallResolver::new(&g_catalog(false))
        .resolve("g", &args)
        .unwrap_err();
    match (err_a, err_b) {
        (
            HostCallResolveError::ArityMismatch {
                actual,
                expected,
                variants,
                ..
            },
            HostCallResolveError::ArityMismatch {
                actual: actual_b,
                expected: expected_b,
                variants: variants_b,
                ..
            },
        ) => {
            assert_eq!(actual, 3);
            assert_eq!(expected, vec![1, 2]);
            assert_eq!(
                variants,
                vec!["g(int)".to_string(), "g(string, string)".to_string()]
            );
            // Reversed registration must produce byte-identical payloads.
            assert_eq!(actual_b, actual);
            assert_eq!(expected_b, expected);
            assert_eq!(variants_b, variants);
        }
        (a, b) => panic!("expected ArityMismatch in both orders, got {a:?} / {b:?}"),
    }
}

#[test]
fn passing_mode_only_overloads_are_ambiguous() {
    // Three `consume` overloads with an identical resource argument shape,
    // differing only in Borrow/BorrowMut/TakeOwned. The call site supplies only
    // a schema and no passing intent, so resolution is ambiguous rather than
    // silently picking by registration order.
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(io_file(), "file"));
    for passing in [
        HostParamPassing::Borrow,
        HostParamPassing::BorrowMut,
        HostParamPassing::TakeOwned,
    ] {
        builder.function(HostFunctionSchema::with_return(
            "consume",
            vec![HostParamSchema::with_passing(
                "h",
                resource(io_file()),
                passing,
            )],
            HostTypeSchema::Int,
        ));
    }
    let catalog = builder
        .build()
        .expect("passing-mode-only overloads are legal");
    let resolver = HostCallResolver::new(&catalog);
    assert!(matches!(
        resolver.resolve("consume", &[res(io_file())]),
        Err(HostCallResolveError::Ambiguous { name, .. }) if name == "consume"
    ));
}
