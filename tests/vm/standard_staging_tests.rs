//! Standard adapter auto-staging: partial-registry completion and the
//! memoized persistent snapshot behind `bind_vm_cached`.
//!
//! These tests drive the standard exact binding path directly:
//!
//! * A registry that already carries standard IO exact entries (a *partial*
//!   standard registry) must auto-complete the missing HTTP / SQLite surfaces
//!   for a program that requires all three, rather than failing `MissingExact`
//!   or re-registering the present IO surface.
//! * A registry with a **custom / mixed-fingerprint** exact entry must not be
//!   silently combined with the standard snapshot: auto-staging is rejected
//!   and the registry stays unchanged.
//! * After the first successful auto-stage, the fully-staged snapshot is
//!   memoized; a second bind performs zero re-registration / generation
//!   change (the registration counter and the snapshot's generation are
//!   stable).

use std::sync::Arc;

use vm::{
    CompileSourceFileOptions, HostFunctionRegistry, SourceFlavor, Vm,
    compile_source_with_flavor_and_options, register_io_builtin_module, standard_host_catalog,
};

/// Compiles `source` against the authoritative combined standard snapshot so
/// every import carries the standard fingerprint.
fn compile_standard(source: &str) -> vm::CompiledProgram {
    let catalog = standard_host_catalog();
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("source should compile against the standard catalog")
}

/// A program exercising IO + HTTP + SQLite surfaces so all three standard
/// adapter namespaces appear in its exact imports.
const IO_HTTP_SQLITE_SOURCE: &str = r#"
    use io;
    use http;
    use sqlite;
    io::exists("/");
    http::client::request({ "method": "GET", "url": "http://127.0.0.1:1/x" });
    let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
    sqlite::close(&db);
"#;

/// A program exercising only the IO surface.
const IO_ONLY_SOURCE: &str = r#"
    use io;
    io::exists("/");
"#;

const HTTP_ONLY_SOURCE: &str = r#"
    use http;
    http::client::request({ "method": "GET", "url": "http://127.0.0.1:1/x" });
"#;

const SQLITE_ONLY_SOURCE: &str = r#"
    use sqlite;
    let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
    sqlite::close(&db);
"#;

// ---------------------------------------------------------------------------
// Finding 4: partial standard registry completion
// ---------------------------------------------------------------------------

/// A registry containing only the standard IO surface (a partial standard
/// registry). Missing HTTP / SQLite adapters must be auto-staged so a program
/// requiring all three surfaces binds exactly.
#[test]
fn partial_io_registry_completes_missing_http_and_sqlite_surfaces() {
    let mut registry = HostFunctionRegistry::new();
    register_io_builtin_module(&mut registry).expect("standard IO registration should succeed");

    let compiled = compile_standard(IO_HTTP_SQLITE_SOURCE);
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");

    registry
        .bind_vm_cached(&mut vm)
        .expect("missing HTTP/SQLite surfaces must be completed and the bind must succeed");

    // The IO surface was already present and must not be re-registered: the
    // registration counter records exactly the HTTP+SQLite completion pass(s).
    assert!(
        registry.standard_staging_snapshot().is_some(),
        "a fully-staged snapshot should have been memoized"
    );
}

/// A custom-fingerprint exact entry coexisting with standard imports must
/// reject auto-staging and leave the registry unchanged — no name-only
/// fallback, no silent combination with the standard snapshot.
#[test]
fn custom_mixed_partial_registry_rejects_and_stays_unchanged() {
    use vm::{HostFunctionSchema, HostTypeSchema};

    let mut registry = HostFunctionRegistry::new();
    // A custom exact entry under a non-standard fingerprint.
    let custom_catalog = {
        let mut builder = vm::HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "custom::marker",
            Vec::new(),
            HostTypeSchema::Int,
        ));
        Arc::new(builder.build().expect("custom catalog must build"))
    };
    let schema = vm::catalog_import_schemas(&custom_catalog, "custom::marker")
        .into_iter()
        .next()
        .expect("one marker schema");
    registry
        .register_exact_static("custom::marker", 0, schema, |_vm, _args| {
            Ok(vm::CallOutcome::Return(vm::CallReturn::one(
                vm::Value::Int(7),
            )))
        })
        .expect("custom exact registration should succeed");

    let generation_before = registry.registry_generation();

    let compiled = compile_standard(IO_ONLY_SOURCE);
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");

    let err = registry
        .bind_vm_cached(&mut vm)
        .expect_err("custom/mixed registry must reject standard auto-staging");

    assert!(
        err.to_string().contains("no exact binding") || err.to_string().contains("MissingExact"),
        "standard IO import must fail resolution on a custom registry: {err}"
    );
    // The registry is unchanged: no snapshot published, generation stable,
    // registration counter untouched.
    assert!(
        registry.standard_staging_snapshot().is_none(),
        "custom/mixed registry must not publish a standard snapshot"
    );
    assert_eq!(
        registry.registry_generation(),
        generation_before,
        "custom/mixed rejection must not perturb the registry generation"
    );
    assert_eq!(
        registry.standard_staging_registrations(),
        0,
        "custom/mixed rejection must not register any standard surface"
    );
}

// ---------------------------------------------------------------------------
// Finding 5: persistent memoized standard staging
// ---------------------------------------------------------------------------

/// The first bind auto-stages the missing standard surface(s) and memoizes the
/// fully-staged snapshot; a second bind on the same registry reuses the
/// snapshot with zero re-registration / generation change.
#[test]
fn second_bind_reuses_memoized_snapshot_with_zero_registration_change() {
    let registry = HostFunctionRegistry::new();
    assert_eq!(registry.standard_staging_registrations(), 0);

    let compiled = compile_standard(IO_ONLY_SOURCE);

    // First bind stages the IO surface.
    let mut vm1 =
        Vm::try_new(compiled.program.clone()).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm1)
        .expect("first bind should auto-stage the IO surface");
    assert_eq!(registry.standard_staging_registrations(), 1);
    let snapshot_after_first = registry
        .standard_staging_snapshot()
        .expect("first bind should publish a snapshot");
    let snapshot_generation_after_first = snapshot_after_first.registry_generation();

    // Second bind reuses the memoized snapshot: no new registration, no
    // generation change on the cached template.
    let mut vm2 =
        Vm::try_new(compiled.program.clone()).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm2)
        .expect("second bind should reuse the memoized snapshot");

    assert_eq!(
        registry.standard_staging_registrations(),
        1,
        "second bind must not perform another standard registration round"
    );
    let snapshot_after_second = registry
        .standard_staging_snapshot()
        .expect("snapshot should persist across binds");
    let snapshot_generation_after_second = snapshot_after_second.registry_generation();
    assert_eq!(
        snapshot_generation_after_second, snapshot_generation_after_first,
        "reusing the memoized snapshot must not bump its generation"
    );
}

#[test]
fn io_first_snapshot_expands_for_later_http_and_sqlite_imports() {
    let registry = HostFunctionRegistry::new();

    for (source, expected_rounds) in [
        (IO_ONLY_SOURCE, 1),
        (HTTP_ONLY_SOURCE, 2),
        (SQLITE_ONLY_SOURCE, 3),
    ] {
        let compiled = compile_standard(source);
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        registry
            .bind_vm_cached(&mut vm)
            .expect("a cached partial snapshot must expand for later standard imports");
        assert_eq!(
            registry.standard_staging_registrations(),
            expected_rounds,
            "each newly required surface is staged exactly once"
        );
    }

    let compiled = compile_standard(IO_HTTP_SQLITE_SOURCE);
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("the expanded snapshot must cover all three surfaces");
    assert_eq!(
        registry.standard_staging_registrations(),
        3,
        "a fully covering cached snapshot must not restage any surface"
    );
}

#[test]
fn standard_snapshot_expansion_is_order_independent() {
    let surfaces = [IO_ONLY_SOURCE, HTTP_ONLY_SOURCE, SQLITE_ONLY_SOURCE];
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let registry = HostFunctionRegistry::new();
        for (round, index) in order.into_iter().enumerate() {
            let compiled = compile_standard(surfaces[index]);
            let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
            registry
                .bind_vm_cached(&mut vm)
                .expect("every standard surface order must bind");
            assert_eq!(
                registry.standard_staging_registrations(),
                (round + 1) as u64,
                "each order stages only its newly required surface"
            );
        }

        let compiled = compile_standard(IO_HTTP_SQLITE_SOURCE);
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        registry
            .bind_vm_cached(&mut vm)
            .expect("the final cached snapshot must cover every surface");
        assert_eq!(registry.standard_staging_registrations(), 3);
    }
}

/// A registry that already fully covers a required surface (IO) never needs a
/// snapshot even after binding, since nothing was auto-staged.
#[test]
fn pre_registered_surface_needs_no_auto_stage() {
    let mut registry = HostFunctionRegistry::new();
    register_io_builtin_module(&mut registry).expect("standard IO registration");

    let compiled = compile_standard(IO_ONLY_SOURCE);
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("pre-registered IO surface should bind");

    assert_eq!(
        registry.standard_staging_registrations(),
        0,
        "no missing surface → no auto-stage registration"
    );
    assert!(
        registry.standard_staging_snapshot().is_none(),
        "no staging happens when every required surface is already present"
    );
}
