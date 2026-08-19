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
//!   program's `HostImport`. There is deliberately no raw fingerprint
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

/// Registers a host extension against the standard host-function registry and
/// installs its persistent module state.
///
/// Used directly by embedders; the `register` / `install` lifecycle is split
/// so an extension can also be registered into a caller-supplied (e.g.
/// restricted / capability-granted) [`HostFunctionRegistry`] by calling
/// [`HostExtension::register`] directly and binding it with
/// [`HostFunctionRegistry::bind_vm_cached`].
pub trait HostExtension: Send + Sync + 'static {
    /// Registers this extension's host functions into `registry`.
    ///
    /// Registration must use the exact schema surfaced from the extension's
    /// [`HostApiCatalog`] (e.g. [`catalog_import_schemas`] plus
    /// `HostFunctionRegistry::register_exact*`); a name-only fallback is not
    /// part of this surface. The default registers nothing.
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
            fingerprint: catalog.fingerprint(),
        })
        .collect()
}
