//! Runtime contract tests for standard host descriptor composition.

use std::collections::BTreeSet;

const HTTP_SURFACE_ENABLED: bool = cfg!(all(feature = "http-client", not(target_family = "wasm")));
const SQLITE_SURFACE_ENABLED: bool = cfg!(all(feature = "sqlite", not(target_family = "wasm")));

const STANDARD_CATALOG_FINGERPRINT_HTTP_SQLITE: &str = "8aad996e0b2f010b";
const STANDARD_CATALOG_FINGERPRINT_SQLITE: &str = "8afd8a69c58f02bd";
const STANDARD_CATALOG_FINGERPRINT_HTTP: &str = "7b558c161322fc76";
const STANDARD_CATALOG_FINGERPRINT_BASE: &str = "8254b00d727494b4";
const IO_CATALOG_FINGERPRINT: &str = "a16730bd11bf5e10";
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
const SQLITE_CATALOG_FINGERPRINT: &str = "dce61460a46c421a";
const JIT_CATALOG_FINGERPRINT: &str = "ae81318e8a018681";
const TIMER_CATALOG_FINGERPRINT: &str = "7ecc0517cea3570b";
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
const HTTP_CATALOG_FINGERPRINT: &str = "329e09ffbdd82a6f";

fn standard_catalog_fingerprint() -> &'static str {
    match (HTTP_SURFACE_ENABLED, SQLITE_SURFACE_ENABLED) {
        (true, true) => STANDARD_CATALOG_FINGERPRINT_HTTP_SQLITE,
        (false, true) => STANDARD_CATALOG_FINGERPRINT_SQLITE,
        (true, false) => STANDARD_CATALOG_FINGERPRINT_HTTP,
        (false, false) => STANDARD_CATALOG_FINGERPRINT_BASE,
    }
}

#[test]
fn standard_catalog_is_derived_from_one_descriptor_per_module() {
    let modules = vm::standard_host_modules();
    assert!(!modules.is_empty(), "the standard host modules must exist");

    let names: Vec<&str> = modules.iter().map(|module| module.name).collect();
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "standard host module names must be unique"
    );
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "standard host module order must be deterministic"
    );

    for module in modules {
        assert!(
            !module.owned.is_empty(),
            "module '{}' must own descriptors",
            module.name
        );
        let Some(surface) = module.catalog_module() else {
            continue;
        };
        assert!(
            !surface.functions.is_empty(),
            "module '{}' published an empty surface",
            module.name
        );
        for factory in surface.functions {
            let descriptor = factory();
            assert!(
                module
                    .owned
                    .iter()
                    .any(|owned| owned().schema.name == descriptor.schema.name),
                "module '{}' publishes `{}` without owning it",
                module.name,
                descriptor.schema.name
            );
        }
    }

    let catalog = vm::standard_host_catalog();
    let merged = vm::standard_catalog_modules();
    assert_eq!(
        merged.len(),
        modules
            .iter()
            .filter(|module| module.catalog_module().is_some())
            .count()
    );
    assert_eq!(
        catalog.functions().len(),
        merged
            .iter()
            .map(|module| module.functions.len())
            .sum::<usize>()
    );
    assert_eq!(
        catalog.fingerprint().to_string(),
        standard_catalog_fingerprint()
    );
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
fn http_catalog_surface() -> Option<(&'static str, &'static str, vm::HostApiFingerprint)> {
    Some((
        "http",
        HTTP_CATALOG_FINGERPRINT,
        vm::http_host_catalog().fingerprint(),
    ))
}

#[cfg(not(all(feature = "http-client", not(target_family = "wasm"))))]
fn http_catalog_surface() -> Option<(&'static str, &'static str, vm::HostApiFingerprint)> {
    None
}

#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
fn sqlite_catalog_surface() -> Option<(&'static str, &'static str, vm::HostApiFingerprint)> {
    Some((
        "sqlite",
        SQLITE_CATALOG_FINGERPRINT,
        vm::sqlite_host_catalog().fingerprint(),
    ))
}

#[cfg(not(all(feature = "sqlite", not(target_family = "wasm"))))]
fn sqlite_catalog_surface() -> Option<(&'static str, &'static str, vm::HostApiFingerprint)> {
    None
}

#[test]
fn module_catalogs_keep_their_published_fingerprints() {
    let mut surfaces = vec![
        (
            "io",
            IO_CATALOG_FINGERPRINT,
            vm::io_host_catalog().fingerprint(),
        ),
        (
            "jit",
            JIT_CATALOG_FINGERPRINT,
            vm::jit_host_catalog().fingerprint(),
        ),
        (
            "timer",
            TIMER_CATALOG_FINGERPRINT,
            vm::timer_host_catalog().fingerprint(),
        ),
    ];
    surfaces.extend(http_catalog_surface());
    surfaces.extend(sqlite_catalog_surface());
    for (name, golden, fingerprint) in surfaces {
        assert_eq!(
            fingerprint.to_string(),
            golden,
            "`{name}` catalog fingerprint changed"
        );
    }
}

#[test]
fn every_catalog_resource_key_has_a_declaration() {
    let catalog = vm::standard_host_catalog();
    assert!(
        catalog
            .resources()
            .iter()
            .all(|resource| !resource.description.trim().is_empty())
    );

    let mut expected = BTreeSet::from(["io.file"]);
    if SQLITE_SURFACE_ENABLED {
        expected.insert("sqlite.connection");
    }
    let actual: BTreeSet<&str> = catalog
        .resources()
        .iter()
        .map(|resource| resource.key.as_str())
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn typed_named_struct_contract_follows_composed_modules() {
    let catalog = vm::standard_host_catalog();
    let declared: BTreeSet<&str> = catalog
        .structs()
        .iter()
        .map(|schema| schema.name.as_str())
        .collect();

    assert!(declared.contains("JitConfig"));
    for name in [
        "SqliteValue",
        "SqliteQueryResult",
        "SqliteTransactionResult",
    ] {
        assert_eq!(declared.contains(name), SQLITE_SURFACE_ENABLED, "{name}");
    }
    for name in [
        "HttpRequest",
        "HttpResponse",
        "SseEvent",
        "SseSummary",
        "SseCallbackAction",
    ] {
        assert_eq!(declared.contains(name), HTTP_SURFACE_ENABLED, "{name}");
    }

    if SQLITE_SURFACE_ENABLED {
        assert_eq!(
            catalog
                .struct_named("SqliteQueryResult")
                .expect("SqliteQueryResult")
                .fields
                .len(),
            4
        );
        assert_eq!(
            catalog
                .struct_named("SqliteValue")
                .expect("SqliteValue")
                .fields
                .len(),
            5
        );
    }

    assert!(
        catalog
            .functions()
            .iter()
            .all(|function| function.return_type != vm::HostTypeSchema::Unknown)
    );
}

#[test]
fn feature_gates_select_whole_modules_deterministically() {
    let names: BTreeSet<&str> = vm::standard_host_modules()
        .iter()
        .map(|module| module.name)
        .collect();
    for always in ["io", "jit", "timer", "regex", "core", "math"] {
        assert!(names.contains(always), "`{always}` must always be composed");
    }
    assert_eq!(names.contains("http"), HTTP_SURFACE_ENABLED);
    assert_eq!(names.contains("sqlite"), SQLITE_SURFACE_ENABLED);

    let catalog = vm::standard_host_catalog();
    let has_http = catalog
        .functions()
        .iter()
        .any(|function| function.name.starts_with("http::"));
    let has_sqlite = catalog
        .functions()
        .iter()
        .any(|function| function.name.starts_with("sqlite::"));
    assert_eq!(has_http, HTTP_SURFACE_ENABLED);
    assert_eq!(has_sqlite, SQLITE_SURFACE_ENABLED);
}

#[test]
fn low_level_registration_apis_remain_executable() {
    let mut builder = vm::HostApiBuilder::new();
    builder.resource(vm::ResourceTypeSchema::new(
        vm::ResourceTypeKey::new("demo.resource").expect("key"),
        "A demo resource",
    ));
    builder.named_struct(vm::HostStructSchema::new("DemoPoint", vec![]));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::call",
        vec![],
        vm::HostTypeSchema::Int,
    ));
    let catalog = builder.build().expect("catalog builds");

    let mut registry = vm::HostFunctionRegistry::empty();
    registry.register_static_stack("demo::call", 0, |_vm, _args| {
        Ok(vm::CallOutcome::Return(vm::CallReturn::None))
    });
    assert!(registry.contains_name("demo::call"));
    assert_eq!(catalog.functions().len(), 1);
}
