//! Public host-extension surface.
//!
//! This module is the controlled extension boundary through which an external
//! host crate installs persistent policy state and registers exact host
//! functions without accessing any [`HostRuntime`](super::host_runtime::HostRuntime)
//! private field or naming a builtin domain module:
//!
//! - [`HostExtension::install`] installs typed per-VM module state (policy /
//!   configuration) through the generic [`HostContext`] module-state store.
//!   That store is owned directly by the host runtime: it persists across
//!   [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse) and execution-scope
//!   close, and it never participates in resource close.
//! - [`HostExtension::register`] registers host functions into a
//!   [`HostFunctionRegistry`] using the exact-schema surface
//!   (`register_exact*`). Exact schemas must be derived from a
//!   [`HostApiCatalog`] via [`catalog_import_schemas`] so the registered
//!   schema — parameter labels, type schemas, passing modes and the **catalog
//!   fingerprint** — is byte-for-byte the identity the compiler embeds in the
//!   program's `HostImport`. Named-struct bodies from
//!   [`HostExtension::catalog`] are installed atomically by
//!   [`register_host_extension`] / [`super::Vm::install_extension`] before
//!   `register`, so VM resource walks can see nested resources while compiler
//!   identity stays `TypeSchema::Named`. There is deliberately no raw fingerprint
//!   constructor here: the fingerprint always comes from
//!   [`HostApiCatalog::fingerprint`](crate::host_api::HostApiCatalog::fingerprint)
//!   and a name-only (schema-less) fallback is never available at this
//!   surface, so unbound exact imports are rejected by the registry with a
//!   structured `MissingExact` error instead of silently matching by name.
//!
//! `src/vm` therefore stays host-agnostic: resource classes, pending
//! operations and module state are supplied by the extension, while the
//! execution scope owns their lifecycle.
//!
//! **Boundary contract:** like [`super::host_context`], this module has no
//! coupling to the builtin runtime modules or any concrete host library.

use crate::bytecode::{HostImportParam, HostImportSchema};
use crate::host_api::HostApiCatalog;
use crate::vm::VmResult;

pub use super::host_context::HostContext;
pub use super::host_context::HostModule as HostModuleState;

/// Public name for the typed per-VM module-state marker used by the external
/// extension surface.
///
/// `HostModuleState` is the stable alias for `HostModule`: a marker bound on
/// a concrete `State` type (keyed by `TypeId`), registered through
/// [`HostContext::set_module_state`] and borrowed through
/// [`HostContext::module_state`] / [`HostContext::module_state_mut`]. State is
/// per-`Vm`, deliberately survives
/// [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse) and execution-scope
/// close, and never participates in resource close.
///
/// Registers a host extension against the standard host-function registry and
/// installs its persistent module state.
///
/// Used directly by embedders; the `register` / `install` lifecycle is split
/// so an extension can also be registered into a caller-supplied (e.g.
/// restricted / capability-granted) [`HostFunctionRegistry`] by calling
/// [`register_host_extension`] and binding it with
/// [`HostFunctionRegistry::bind_vm_cached`].
pub trait HostExtension: Send + Sync + 'static {
    /// Catalog whose named-struct bodies are installed atomically before
    /// [`Self::register`] by [`super::Vm::install_extension`] and
    /// [`register_host_extension`]. Compiler import identity stays
    /// `TypeSchema::Named`. The default is none.
    fn catalog(&self) -> Option<&HostApiCatalog> {
        None
    }

    /// Registers this extension's host functions into `registry`.
    ///
    /// Registration must use the exact schema surfaced from the extension's
    /// [`HostApiCatalog`] (e.g. [`catalog_import_schemas_into`] or
    /// [`catalog_import_schemas`] plus
    /// `HostFunctionRegistry::register_exact*`); a name-only fallback is not
    /// part of this surface. Prefer [`register_host_extension`] or
    /// [`super::Vm::install_extension`] so catalog named-struct bodies are
    /// installed before exact registration. The default registers nothing.
    fn register(&self, registry: &mut super::host::HostFunctionRegistry) -> VmResult<()> {
        let _ = registry;
        Ok(())
    }

    /// Installs this extension's persistent per-VM module state.
    ///
    /// Typed state installed here (through
    /// [`HostContext::set_module_state`]) survives
    /// [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse) and scope close and
    /// never participates in resource close.
    ///
    /// **Infallible by design.** The module-state install phase performs no
    /// fallible operations (the state store is infallible), so
    /// [`Vm::install_extension`](super::Vm::install_extension) can guarantee
    /// transactional failure semantics: every fallible step (registration and
    /// registry binding) runs *before* this method, and once it runs the VM is
    /// fully and consistently installed. Extensions that need a fallible
    /// initialization step must perform it in [`Self::register`] instead, so
    /// the failure surfaces before any install mutation. The default installs
    /// nothing.
    fn install(&self, vm: &mut super::Vm) {
        let _ = vm;
    }
}

/// Converts every catalog-declared overload of `name` into the exact
/// [`HostImportSchema`] the compiler embeds at a call site.
///
/// The produced schemas carry the declared parameter labels, `TypeSchema`s,
/// passing modes, return schema and the catalog's own
/// [`HostApiCatalog::fingerprint`](crate::host_api::HostApiCatalog::fingerprint)
/// — exactly the identity stored in a `HostImport`'s schema during codegen
/// when the same catalog is supplied to the compiler. Registering these
/// schemas (via `HostFunctionRegistry::register_exact*`) therefore satisfies
/// the exact-schema registry lookup with no drift and no raw fingerprint
/// construction on the host side.
pub fn catalog_import_schemas(catalog: &HostApiCatalog, name: &str) -> Vec<HostImportSchema> {
    let fingerprint = catalog.fingerprint();
    catalog_import_schemas_with_fingerprint(catalog, name, fingerprint)
}

/// Named struct bodies for VM resource walks. Compiler import schemas keep
/// `TypeSchema::Named` identity; install these on
/// [`HostFunctionRegistry::install_named_struct_schemas`] so exact registration
/// can see nested resources inside named structs.
pub fn catalog_named_struct_schemas(
    catalog: &HostApiCatalog,
) -> std::collections::HashMap<String, crate::bytecode::NamedStructSchema> {
    catalog
        .structs()
        .iter()
        .map(|schema| {
            (
                schema.name.clone(),
                crate::bytecode::NamedStructSchema {
                    type_params: Vec::new(),
                    body_schema: schema.to_compiler_object_schema(),
                },
            )
        })
        .collect()
}

/// Installs catalog named-struct bodies onto `registry`, then returns exact
/// import schemas for `name`. Use this from [`HostExtension::register`] so
/// resource walks see nested `TypeSchema::Named` bodies without a separate
/// test-only table install. Compiler import identity stays `TypeSchema::Named`.
pub fn catalog_import_schemas_into(
    registry: &mut super::host::HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
) -> Vec<HostImportSchema> {
    registry.install_named_struct_schemas(catalog_named_struct_schemas(catalog));
    catalog_import_schemas(catalog, name)
}

/// Registers `extension` after atomically installing named-struct bodies from
/// [`HostExtension::catalog`]. Restricted-registry callers should use this
/// instead of calling [`HostExtension::register`] directly.
pub fn register_host_extension(
    registry: &mut super::host::HostFunctionRegistry,
    extension: &dyn HostExtension,
) -> VmResult<()> {
    if let Some(catalog) = extension.catalog() {
        registry.install_named_struct_schemas(catalog_named_struct_schemas(catalog));
    }
    extension.register(registry)
}

fn catalog_import_schemas_with_fingerprint(
    catalog: &HostApiCatalog,
    name: &str,
    fingerprint: crate::host_api::HostApiFingerprint,
) -> Vec<HostImportSchema> {
    catalog
        .functions_named(name)
        .into_iter()
        .map(|function| HostImportSchema {
            params: function
                .params
                .iter()
                .map(|param| HostImportParam {
                    name: param.name.clone(),
                    schema: param.ty.to_compiler_schema(),
                    passing: param.passing,
                })
                .collect(),
            return_type: function.return_type.to_compiler_schema(),
            fingerprint,
        })
        .collect()
}

/// Validates the adapter ABI for one required catalog member before registry
/// mutation. The member must exist and match one of the canonical adapter
/// overloads in parameter labels, passing modes, parameter schemas and return
/// schema. Catalog fingerprints are deliberately ignored so custom and
/// combined catalogs remain usable.
pub fn validate_catalog_import_schemas(
    catalog: &HostApiCatalog,
    contract: &HostApiCatalog,
    name: &str,
) -> VmResult<Vec<HostImportSchema>> {
    validate_catalog_import_schemas_with_fingerprints(
        catalog,
        contract,
        name,
        catalog.fingerprint(),
        contract.fingerprint(),
    )
}

/// Validates one adapter member using fingerprints computed once by a
/// registration pass. Adapter contract tables use this to avoid recomputing a
/// catalog fingerprint for every overload/member.
pub fn validate_catalog_import_schemas_with_fingerprints(
    catalog: &HostApiCatalog,
    contract: &HostApiCatalog,
    name: &str,
    catalog_fingerprint: crate::host_api::HostApiFingerprint,
    contract_fingerprint: crate::host_api::HostApiFingerprint,
) -> VmResult<Vec<HostImportSchema>> {
    let expected = catalog_import_schemas_with_fingerprint(contract, name, contract_fingerprint);
    let got = catalog_import_schemas_with_fingerprint(catalog, name, catalog_fingerprint);
    if got.is_empty() {
        return Err(crate::vm::VmError::HostImportBinding(
            crate::vm::HostImportBindingError::MissingCatalogMember {
                import: name.to_string(),
                expected,
            },
        ));
    }

    let compatible = |expected: &HostImportSchema, got: &HostImportSchema| {
        expected.params == got.params && expected.return_type == got.return_type
    };
    let all_expected_match = expected
        .iter()
        .all(|expected| got.iter().any(|got| compatible(expected, got)));
    let all_got_match = got
        .iter()
        .all(|got| expected.iter().any(|expected| compatible(expected, got)));
    if expected.len() != got.len() || !all_expected_match || !all_got_match {
        return Err(crate::vm::VmError::HostImportBinding(
            crate::vm::HostImportBindingError::IncompatibleCatalogSchema {
                import: name.to_string(),
                expected,
                got,
            },
        ));
    }
    Ok(got)
}
