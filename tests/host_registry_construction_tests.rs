//! Registry construction and composition behavior for the host-agnostic scope
//! refactor.
//!
//! `HostFunctionRegistry`'s primitive constructor is `empty()` — a bare,
//! host-agnostic registry with no default host functions and no standard
//! composition. The *standard-composed* variants (`new()`, `Default`,
//! `restricted()`) physically live in the outer builtin/runtime layer and
//! delegate to the builtin registrar, so the VM core never owns a
//! builtin-composed process-global default template.
//!
//! These behavior tests pin the public contract:
//!
//! * `empty()` carries no builtin-composed default and no standard
//!   composition (a standard-import program cannot be auto-staged against it).
//! * `new()` / `Default` carry the default host functions and standard
//!   composition so a standard-import program binds and runs.
//! * `restricted()` carries the standard surfaces but requires an explicit
//!   capability grant before binding.
//! * A caller-provided composition installed through `set_standard_composition`
//!   drives auto-staging on a bare registry.
//! * Replacing the composition invalidates the memoized staging snapshot, so a
//!   second bind under a *new* composition cannot reuse the previous
//!   composition's staged snapshot.

use std::sync::Arc;

use vm::{
    CapabilityProfile, HostFunctionRegistry, SourceFlavor, Vm,
    compile_source_with_flavor_and_options, standard_composition, standard_host_catalog,
};

fn compile_standard(source: &str) -> vm::CompiledProgram {
    let catalog = standard_host_catalog();
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("source should compile against the standard catalog")
}

/// A standard adapter-surface program (the IO surface is staged by the
/// standard composition).
fn io_program() -> vm::CompiledProgram {
    compile_standard("use io; io::exists(\"/\");")
}

/// `empty()` is a bare, host-agnostic registry: no default host functions and
/// no standard composition, so a standard adapter-surface import cannot be
/// bound or auto-staged against it.
#[test]
fn empty_registry_is_bare_and_has_no_standard_composition() {
    let registry = HostFunctionRegistry::empty();
    assert_eq!(registry.plan_cache_len(), 0);
    assert!(registry.standard_staging_snapshot().is_none());

    let compiled = io_program();
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let err = registry
        .bind_vm_cached(&mut vm)
        .expect_err("a bare empty registry must not bind a standard-import program");
    assert!(
        err.to_string().contains("no exact binding") || err.to_string().contains("MissingExact"),
        "bare registry must fail standard resolution: {err}"
    );
}

/// `new()` carries the default host functions and caller-provided standard
/// composition, so a standard adapter-surface import binds (stages) against it.
#[test]
fn new_registry_carries_standard_surfaces() {
    let registry = HostFunctionRegistry::new();
    let compiled = io_program();
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("default registry must bind the standard IO import");
}

/// `Default` matches `new()`: both expose the standard-composed registry.
#[test]
fn default_matches_standard_registry() {
    let registry = HostFunctionRegistry::default();
    let compiled = io_program();
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("default registry must bind the standard IO import");
}

/// `restricted()` carries the standard surfaces but requires an explicit
/// capability grant before the standard import binds.
#[test]
fn restricted_registry_requires_explicit_grant_for_standard_import() {
    let registry = HostFunctionRegistry::restricted();
    let compiled = io_program();
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let err = registry
        .bind_vm_cached(&mut vm)
        .expect_err("restricted registry must reject an ungranted standard import");
    assert!(
        err.to_string().contains("capability profile"),
        "restricted must surface the capability-profile rejection: {err}"
    );

    let mut granted = HostFunctionRegistry::restricted();
    let profile = CapabilityProfile::builder()
        .allow_host_import("io::exists")
        .build();
    granted.set_capability_profile(profile);
    let mut vm = Vm::try_new(io_program().program).expect("test VM construction must not fail");
    granted
        .bind_vm_cached(&mut vm)
        .expect("granted restricted registry must bind the standard import");
}

/// A caller-provided composition installed through `set_standard_composition`
/// turns a bare registry into a staging-capable one.
#[test]
fn installing_composition_enables_staging_on_bare_registry() {
    let mut registry = HostFunctionRegistry::empty();
    registry.set_standard_composition(standard_composition());

    let compiled = io_program();
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("explicit composition must enable standard auto-staging on a bare registry");
}

// ---------------------------------------------------------------------------
// Finding: composition replacement invalidates the memoized staging snapshot
// ---------------------------------------------------------------------------

/// A custom composition that stages a distinct concrete surface under a
/// distinct catalog fingerprint, so a registry bound under it cannot be reused
/// for the standard snapshot and vice versa.
struct CustomComposition;

impl vm::StandardSurfaceComposition for CustomComposition {
    fn standard_catalog_fingerprint(&self) -> vm::HostApiFingerprint {
        custom_catalog().fingerprint()
    }

    fn import_in_standard(&self, import: &vm::HostImport) -> bool {
        let Some(schema) = import.schema.as_ref() else {
            return false;
        };
        schema.fingerprint == self.standard_catalog_fingerprint()
            && !custom_catalog().functions_named(&import.name).is_empty()
    }

    fn ensure_surfaces(
        &self,
        imports: &[vm::HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> vm::VmResult<bool> {
        let catalog = custom_catalog();
        let mut staged = false;
        for import in imports {
            if !self.import_in_standard(import) {
                continue;
            }
            for schema in vm::catalog_import_schemas(&catalog, &import.name) {
                registry.register_exact_static(&import.name, 0, schema, custom_ping)?;
                staged = true;
            }
        }
        Ok(staged)
    }

    fn build_default_registry(&self) -> vm::VmResult<HostFunctionRegistry> {
        let mut registry = HostFunctionRegistry::empty();
        let catalog = custom_catalog();
        for schema in vm::catalog_import_schemas(&catalog, "custom::ping") {
            registry.register_exact_static("custom::ping", 0, schema, custom_ping)?;
        }
        Ok(registry)
    }

    fn bind_default_name(&self, _vm: &mut Vm, _name: &str) -> bool {
        false
    }
}

fn custom_ping(_vm: &mut Vm, _args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::one(
        vm::Value::Int(7),
    )))
}

fn custom_catalog() -> Arc<vm::HostApiCatalog> {
    static CUSTOM: std::sync::OnceLock<Arc<vm::HostApiCatalog>> = std::sync::OnceLock::new();
    CUSTOM
        .get_or_init(|| {
            let mut builder = vm::HostApiBuilder::new();
            builder.function(vm::HostFunctionSchema::with_return(
                "custom::ping",
                Vec::new(),
                vm::HostTypeSchema::Int,
            ));
            Arc::new(builder.build().expect("custom catalog must build"))
        })
        .clone()
}

fn compile_custom(source: &str) -> vm::CompiledProgram {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(custom_catalog()),
    )
    .expect("source should compile against the custom catalog")
}

/// Replacing the composition on a registry invalidates the memoized staging
/// snapshot and plan cache: a second bind under a *new* distinct composition
/// cannot reuse the first composition's staged snapshot. Each bind is proved
/// by its own distinct fingerprint resolving.
#[test]
fn replacing_composition_invalidates_staged_snapshot() {
    // A registry that starts bare but is bound under the standard composition:
    // the first bind auto-stages and memoizes a *standard* snapshot.
    let mut registry = HostFunctionRegistry::empty();
    registry.set_standard_composition(standard_composition());
    let mut vm = Vm::try_new(io_program().program).expect("VM");
    registry
        .bind_vm_cached(&mut vm)
        .expect("first bind should build a standard staged snapshot");
    assert!(
        registry.standard_staging_snapshot().is_some(),
        "first standard bind should memoize a snapshot"
    );

    // Replacing the composition with a distinct custom policy must invalidate
    // the cached standard snapshot and the plan cache so a subsequent bind
    // resolves under the new composition, not the stale standard snapshot.
    let snapshot_before = registry
        .standard_staging_snapshot()
        .map(|r| r.registry_generation());
    registry.set_standard_composition(Arc::new(CustomComposition));
    let stale = registry
        .standard_staging_snapshot()
        .map(|r| r.registry_generation());
    assert_ne!(
        stale, snapshot_before,
        "composition replacement must invalidate the memoized staging snapshot"
    );

    // A program compiled against the custom catalog must now bind under the
    // custom composition; the stale standard snapshot cannot satisfy it.
    let custom_program = compile_custom("use custom; custom::ping();");
    let mut vm = Vm::try_new(custom_program.program).expect("VM");
    registry
        .bind_vm_cached(&mut vm)
        .expect("second bind under the new composition must stage the custom surface, not reuse the stale standard snapshot");
}
