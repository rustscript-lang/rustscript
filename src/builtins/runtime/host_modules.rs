//! Deterministic standard host-module aggregation.
//!
//! Every standard `#[pd_host_function]` belongs to exactly one **standard host
//! module** declared here. A module lists its functions explicitly; there is no
//! linker inventory, no implicit global registration, and no second catalog.
//! Each function's schema, binding class, adapter, guest resource effects, and
//! hidden host-state effects come from its own macro expansion, and a module
//! that was published as a guest-visible host catalog derives that catalog from
//! the same descriptors.
//!
//! Feature gates select modules deterministically: the aggregation builds one
//! ordered list, and a disabled feature removes exactly its module.
//!
//! Two lists are kept apart on purpose:
//!
//! * [`StandardHostModule::catalog`] is the **guest catalog surface**: the
//!   functions the standard [`standard_host_catalog`](super::standard_host_catalog)
//!   publishes. It is the compatibility surface compiled guest code binds
//!   against, so it must reproduce the published catalog byte-for-byte.
//! * [`StandardHostModule::owned`] is **every** standard host function the
//!   module owns, including the ones still dispatched through the generated
//!   namespaced-builtin path. Ownership is exhaustive so the architecture guard
//!   can prove that no standard host function is unowned or double-owned.

use std::sync::{Arc, OnceLock};

use crate::host_api::{HostApiBuilder, HostApiCatalog, HostStructSchema};
use crate::host_extension::HostModuleDescriptor;
use crate::host_extension::{HostFunctionDescriptor, HostResourceTypeMeta};

/// One standard host module: its guest catalog surface and its full ownership
/// list.
pub struct StandardHostModule {
    /// Stable module identity used in diagnostics.
    pub name: &'static str,
    /// The guest-facing catalog surface.
    ///
    /// Returns an empty module for descriptor-only modules that are still
    /// dispatched through the generated namespaced-builtin path.
    pub catalog: fn() -> HostModuleDescriptor,
    /// Every standard host function this module owns, in declaration order.
    pub owned: &'static [fn() -> HostFunctionDescriptor],
    /// The module's named-struct declarations, in declaration order.
    ///
    /// The *bodies* of these structs come from the function descriptors; this
    /// list declares their order and their documentation. Neither order nor
    /// documentation participates in a fingerprint or in an import identity,
    /// but both are part of the surface a tool renders, so they are declared
    /// once here instead of being left to the descriptor walk.
    pub named_structs: &'static [(&'static str, &'static str)],
}

impl StandardHostModule {
    /// The module's guest catalog surface, when it publishes one.
    pub fn catalog_module(&self) -> Option<HostModuleDescriptor> {
        let module = (self.catalog)();
        (!module.functions.is_empty()).then_some(module)
    }

    /// Materializes every owned function descriptor in declaration order.
    pub fn owned_descriptors(&self) -> Vec<HostFunctionDescriptor> {
        self.owned.iter().map(|factory| factory()).collect()
    }
}

/// A module that owns host functions but publishes no guest catalog surface.
///
/// The functions are still single-source descriptors; they are dispatched by
/// the generated namespaced-builtin path, so there is no catalog to derive.
pub(super) const fn descriptor_only_module(
    name: &'static str,
    owned: &'static [fn() -> HostFunctionDescriptor],
) -> StandardHostModule {
    StandardHostModule {
        name,
        catalog: empty_catalog_module,
        owned,
        named_structs: &[],
    }
}

fn empty_catalog_module() -> HostModuleDescriptor {
    HostModuleDescriptor {
        name: EMPTY_MODULE_NAME,
        functions: &[],
        resources: &[],
    }
}

const EMPTY_MODULE_NAME: &str = "<descriptor-only>";

/// A guest catalog surface that publishes exactly the listed descriptors.
pub(super) fn catalog_module(
    name: &'static str,
    functions: &'static [fn() -> HostFunctionDescriptor],
    resources: &'static [fn() -> HostResourceTypeMeta],
) -> HostModuleDescriptor {
    HostModuleDescriptor {
        name,
        functions,
        resources,
    }
}

/// Applies the module's named-struct declaration order and documentation to a
/// derived catalog.
///
/// The derived catalog carries every named struct a descriptor mentions; this
/// restores the order and documentation the published surface used, so a
/// descriptor migration cannot silently reorder a catalog a tool renders.
pub(super) fn declare_named_structs(
    catalog: &HostApiCatalog,
    declarations: &'static [(&'static str, &'static str)],
) -> HostApiCatalog {
    if declarations.is_empty() {
        return catalog.clone();
    }
    let mut ordered: Vec<HostStructSchema> = Vec::with_capacity(catalog.structs().len());
    for (name, description) in declarations {
        if let Some(schema) = catalog.structs().iter().find(|schema| schema.name == *name) {
            ordered.push(schema.clone().with_description(*description));
        }
    }
    // A derived struct that no declaration names keeps its derived position
    // after the declared ones; the architecture guard fails when that happens
    // for a standard module.
    for schema in catalog.structs() {
        if !ordered.iter().any(|declared| declared.name == schema.name) {
            ordered.push(schema.clone());
        }
    }
    let mut builder = HostApiBuilder::new();
    for resource in catalog.resources() {
        builder.resource(resource.clone());
    }
    for schema in ordered {
        builder.named_struct(schema);
    }
    for function in catalog.functions() {
        builder.function(function.clone());
    }
    builder
        .build()
        .expect("a declared standard catalog must stay valid")
}

/// Every standard host module for this build, in deterministic order.
///
/// Feature gates include or exclude whole modules; the order of the returned
/// list is fixed by this function alone.
pub fn standard_host_modules() -> &'static [StandardHostModule] {
    static MODULES: OnceLock<Vec<StandardHostModule>> = OnceLock::new();
    MODULES.get_or_init(|| {
        let mut modules: Vec<StandardHostModule> = vec![
            super::aot::aot_host_module(),
            super::bytes::bytes_host_module(),
            super::context_host::context_host_module(),
            super::core::core_host_module(),
            super::host::runtime_host_module(),
            super::jit::jit_host_module(),
            super::json::json_host_module(),
            super::math::math_host_module(),
            super::regex::regex_host_module(),
        ];
        #[cfg(all(feature = "http-client", not(target_family = "wasm")))]
        modules.push(super::http::http_host_module());
        modules.push(super::io::io_host_module());
        #[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
        modules.push(super::sqlite_schema::sqlite_standard_host_module());
        modules.push(super::timer::timer_host_module());
        modules.sort_by_key(|module| module.name);
        modules
    })
}

/// The standard modules that publish a guest catalog surface for this build.
pub fn standard_catalog_modules() -> Vec<HostModuleDescriptor> {
    standard_host_modules()
        .iter()
        .filter_map(StandardHostModule::catalog_module)
        .collect()
}

/// Builds the standard guest catalog from the module descriptors.
///
/// Resource declarations, named structs, and function schemas all come from the
/// owning module's descriptors. The merge order is the aggregation order; the
/// catalog fingerprint is order-independent, so this is a pure single-source
/// derivation of the previously hand-written surface.
pub(super) fn build_standard_host_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    for module in standard_host_modules() {
        let Some(surface) = module.catalog_module() else {
            continue;
        };
        let catalog = declare_named_structs(
            &surface
                .catalog()
                .unwrap_or_else(|error| panic!("standard host module '{}': {error}", module.name)),
            module.named_structs,
        );
        for resource in catalog.resources() {
            builder.resource(resource.clone());
        }
        for schema in catalog.structs() {
            builder.named_struct(schema.clone());
        }
        for function in catalog.functions() {
            builder.function(function.clone());
        }
    }
    builder
        .build()
        .expect("the standard host catalog must be valid")
}

/// The shared derived catalog of one standard module surface.
pub(super) fn module_catalog(
    name: &'static str,
    functions: &'static [fn() -> HostFunctionDescriptor],
    resources: &'static [fn() -> HostResourceTypeMeta],
    declarations: &'static [(&'static str, &'static str)],
) -> Arc<HostApiCatalog> {
    let catalog = catalog_module(name, functions, resources)
        .catalog()
        .unwrap_or_else(|error| panic!("{name} host catalog must be valid: {error}"));
    Arc::new(declare_named_structs(&catalog, declarations))
}
