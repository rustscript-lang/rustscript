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
use crate::compiler::TypeSchema;
use crate::host_api::{HostApiCatalog, HostTypeSchema};
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
    /// never participates in resource close. The default installs nothing.
    fn install(&self, vm: &mut super::Vm) -> VmResult<()> {
        let _ = vm;
        Ok(())
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
                    schema: into_type_schema(&param.ty),
                    passing: param.passing,
                })
                .collect(),
            return_type: into_type_schema(&function.return_type),
            fingerprint: catalog.fingerprint(),
        })
        .collect()
}

/// Maps a catalog type schema onto the compiler `TypeSchema` used inside an
/// exact `HostImportSchema`. Resources map by their shared `ResourceTypeKey`.
fn into_type_schema(ty: &HostTypeSchema) -> TypeSchema {
    match ty {
        HostTypeSchema::Unknown => TypeSchema::Unknown,
        HostTypeSchema::Null => TypeSchema::Null,
        HostTypeSchema::Int => TypeSchema::Int,
        HostTypeSchema::Float => TypeSchema::Float,
        HostTypeSchema::Number => TypeSchema::Number,
        HostTypeSchema::Bool => TypeSchema::Bool,
        HostTypeSchema::String => TypeSchema::String,
        HostTypeSchema::Bytes => TypeSchema::Bytes,
        HostTypeSchema::Array(inner) => TypeSchema::Array(Box::new(into_type_schema(inner))),
        HostTypeSchema::Map(inner) => TypeSchema::Map(Box::new(into_type_schema(inner))),
        HostTypeSchema::Optional(inner) => TypeSchema::Optional(Box::new(into_type_schema(inner))),
        HostTypeSchema::Callable { params, result } => TypeSchema::Callable {
            params: params.iter().map(into_type_schema).collect(),
            result: Box::new(into_type_schema(result)),
        },
        HostTypeSchema::Resource(key) => TypeSchema::Resource(key.clone()),
    }
}
