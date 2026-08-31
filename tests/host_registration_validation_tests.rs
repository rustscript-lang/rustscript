#![cfg(feature = "runtime")]

use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::{
    HostApiBuilder, HostApiCatalog, HostFunctionRegistry, HostFunctionSchema, HostImport,
    HostImportBindingError, HostTypeSchema, ValueType, VmError, catalog_import_schemas,
};

fn coarse_return_type(schema: &HostTypeSchema) -> ValueType {
    match schema {
        HostTypeSchema::Null => ValueType::Null,
        HostTypeSchema::Int => ValueType::Int,
        HostTypeSchema::Float => ValueType::Float,
        HostTypeSchema::Bool => ValueType::Bool,
        HostTypeSchema::String => ValueType::String,
        HostTypeSchema::Bytes => ValueType::Bytes,
        HostTypeSchema::Array(_) => ValueType::Array,
        HostTypeSchema::Map(_) | HostTypeSchema::Object(_) => ValueType::Map,
        HostTypeSchema::Optional(inner) => coarse_return_type(inner),
        HostTypeSchema::Callable { .. } => ValueType::Callable,
        HostTypeSchema::Unknown | HostTypeSchema::Number | HostTypeSchema::Resource(_) => {
            ValueType::Unknown
        }
    }
}

fn without_function(base: &HostApiCatalog, removed: &str) -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    for resource in base.resources() {
        builder.resource(resource.clone());
    }
    for function in base.functions() {
        if function.name != removed {
            builder.function(function.clone());
        }
    }
    Arc::new(builder.build().expect("catalog variant must build"))
}

fn with_incompatible_write_schema(base: &HostApiCatalog) -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    for resource in base.resources() {
        builder.resource(resource.clone());
    }
    for function in base.functions() {
        if function.name == "io::write" {
            let mut incompatible = function.clone();
            incompatible.params.pop();
            incompatible.return_type = vm::HostTypeSchema::Bool;
            builder.function(incompatible);
        } else {
            builder.function(function.clone());
        }
    }
    Arc::new(
        builder
            .build()
            .expect("incompatible catalog variant must build"),
    )
}
fn with_extra_function(base: &HostApiCatalog) -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    for resource in base.resources() {
        builder.resource(resource.clone());
    }
    for function in base.functions() {
        builder.function(function.clone());
    }
    builder.function(HostFunctionSchema::with_return(
        "custom::marker",
        Vec::new(),
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("combined catalog must build"))
}

fn assert_io_probe_not_partially_registered(catalog: &HostApiCatalog, probe: &str) {
    let source =
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::new(catalog.clone()));
    let compiled = compile_source_with_flavor_and_options(
        &format!("use io; io::{probe}(\"missing.txt\", \"r\");"),
        SourceFlavor::RustScript,
        source,
    )
    .expect("remaining IO member should compile");
    let mut registry = HostFunctionRegistry::empty();
    let before_cache = registry.plan_cache_len();
    let before_generation = registry.registry_generation();
    let result = vm::register_io_builtin_module_from_catalog(&mut registry, catalog);
    let registration_error = result.expect_err("missing catalog member must reject registration");
    assert!(
        matches!(
            &registration_error,
            VmError::HostImportBinding(HostImportBindingError::MissingCatalogMember { .. })
        ),
        "missing member must use structured catalog error: {registration_error:?}"
    );
    assert_eq!(
        registry.plan_cache_len(),
        before_cache,
        "failed registration must not publish staged plans"
    );
    assert_eq!(
        registry.registry_generation(),
        before_generation,
        "failed registration must not advance the registry revision"
    );
    let error = registry.prepare_plan(&compiled.program.imports).expect_err(
        "a failed registration must not leave io::open registered as a partial side effect",
    );
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::MissingExact { .. })
        ),
        "partial-registration probe should report a typed binding miss: {error:?}"
    );
}

#[test]
fn io_registration_rejects_missing_first_member_atomically() {
    let catalog = vm::io_host_catalog();
    let reduced = without_function(&catalog, "io::open");
    assert_io_probe_not_partially_registered(&reduced, "popen");
}

#[test]
fn io_registration_rejects_missing_middle_member_atomically() {
    let catalog = vm::io_host_catalog();
    let reduced = without_function(&catalog, "io::write");
    assert_io_probe_not_partially_registered(&reduced, "open");
}

#[test]
fn io_registration_rejects_missing_last_member_atomically() {
    let catalog = vm::io_host_catalog();
    let reduced = without_function(&catalog, "io::exists");
    assert_io_probe_not_partially_registered(&reduced, "open");
}

#[test]
fn io_registration_rejects_incompatible_adapter_schema() {
    let catalog = vm::io_host_catalog();
    let incompatible = with_incompatible_write_schema(&catalog);
    let mut registry = HostFunctionRegistry::empty();
    let error = vm::register_io_builtin_module_from_catalog(&mut registry, &incompatible)
        .expect_err("adapter-incompatible schema must reject registration");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::IncompatibleCatalogSchema { .. })
        ),
        "schema mismatch must use structured host-import binding error: {error:?}"
    );
}

#[test]
fn io_registration_retry_with_corrected_catalog_publishes_all_exact_members() {
    let catalog = vm::io_host_catalog();
    let reduced = without_function(&catalog, "io::close");
    let mut registry = HostFunctionRegistry::empty();
    assert!(vm::register_io_builtin_module_from_catalog(&mut registry, &reduced).is_err());
    vm::register_io_builtin_module_from_catalog(&mut registry, &catalog)
        .expect("corrected catalog should retry successfully");

    let probe_options =
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
    let probe = compile_source_with_flavor_and_options(
        "use io; io::exists(\".\");",
        SourceFlavor::RustScript,
        probe_options,
    )
    .expect("corrected catalog probe should compile");
    registry
        .prepare_plan(&probe.program.imports)
        .expect("corrected catalog exact probe should prepare");

    let mut all_exact_imports = Vec::new();
    for function in catalog.functions() {
        let return_type = coarse_return_type(&function.return_type);
        for schema in catalog_import_schemas(&catalog, &function.name) {
            all_exact_imports.push(HostImport {
                name: function.name.clone(),
                arity: schema.params.len() as u8,
                return_type,
                schema: Some(schema),
            });
        }
    }
    registry
        .prepare_plan(&all_exact_imports)
        .expect("every corrected IO catalog schema should bind exactly");

    for name in [
        "io::open",
        "io::popen",
        "io::read_all",
        "io::read_line",
        "io::write",
        "io::flush",
        "io::close",
        "io::exists",
    ] {
        assert!(
            !catalog_import_schemas(&catalog, name).is_empty(),
            "catalog member {name}"
        );
    }
}

#[test]
fn io_registration_accepts_custom_combined_catalog_identity() {
    let standard = vm::io_host_catalog();
    let combined = with_extra_function(&standard);
    assert_ne!(combined.fingerprint(), standard.fingerprint());
    let options = CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&combined));
    let compiled = compile_source_with_flavor_and_options(
        "use io; io::exists(\".\");",
        SourceFlavor::RustScript,
        options,
    )
    .expect("custom combined catalog compile");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "io::exists")
        .expect("exact IO import");
    assert_eq!(
        import.schema.as_ref().unwrap().fingerprint,
        combined.fingerprint()
    );
    let mut registry = HostFunctionRegistry::empty();
    vm::register_io_builtin_module_from_catalog(&mut registry, &combined)
        .expect("custom combined catalog registration");
    registry
        .prepare_plan(&compiled.program.imports)
        .expect("custom combined exact schema should bind");
}

#[test]
fn io_registration_duplicate_conflict_remains_typed_and_atomic() {
    let catalog = vm::io_host_catalog();
    let mut registry = HostFunctionRegistry::empty();
    vm::register_io_builtin_module_from_catalog(&mut registry, &catalog)
        .expect("first registration");
    let before_cache = registry.plan_cache_len();
    let before_generation = registry.registry_generation();
    let error = vm::register_io_builtin_module_from_catalog(&mut registry, &catalog)
        .expect_err("duplicate exact registration must reject");
    assert!(matches!(
        error,
        VmError::HostImportBinding(HostImportBindingError::Duplicate { .. })
    ));
    assert_eq!(
        registry.plan_cache_len(),
        before_cache,
        "duplicate failure must be atomic"
    );
    assert_eq!(
        registry.registry_generation(),
        before_generation,
        "duplicate failure must not advance the registry revision"
    );
}

#[cfg(feature = "http-client")]
#[test]
fn http_registration_rejects_missing_request_and_sse_members_atomically() {
    let catalog = vm::http_host_catalog();
    for removed in ["http::client::request", "http::client::sse"] {
        let reduced = without_function(&catalog, removed);
        let mut registry = HostFunctionRegistry::empty();
        let before_generation = registry.registry_generation();
        let error = vm::register_http_builtin_module_from_catalog(&mut registry, &reduced)
            .expect_err("missing HTTP member must reject registration");
        assert!(matches!(
            &error,
            VmError::HostImportBinding(HostImportBindingError::MissingCatalogMember { .. })
        ));
        assert_eq!(registry.plan_cache_len(), 0);
        assert_eq!(registry.registry_generation(), before_generation);
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_registration_rejects_missing_static_and_pending_members_atomically() {
    let catalog = vm::sqlite_host_catalog();
    for removed in ["sqlite::open", "sqlite::query", "sqlite::next_cursor"] {
        let reduced = without_function(&catalog, removed);
        let mut registry = HostFunctionRegistry::empty();
        let before_generation = registry.registry_generation();
        let error = vm::register_sqlite_builtin_module_from_catalog(&mut registry, &reduced)
            .expect_err("missing SQLite member must reject registration");
        assert!(matches!(
            &error,
            VmError::HostImportBinding(HostImportBindingError::MissingCatalogMember { .. })
        ));
        assert_eq!(registry.plan_cache_len(), 0);
        assert_eq!(registry.registry_generation(), before_generation);
    }
}
