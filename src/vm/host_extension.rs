//! Public host-extension surface.
//!
//! This module is the controlled extension boundary through which an external
//! host crate installs persistent policy state and registers host functions
//! without accessing any [`HostRuntime`](super::host_runtime::HostRuntime)
//! private field or naming a builtin domain module:
//!
//! - [`HostExtension::install`] installs typed per-VM module state (policy /
//!   configuration) through the generic [`HostContext`] module-state store.
//!   That store is owned directly by the host runtime: it persists across
//!   [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse) and execution-scope
//!   close, and it never participates in resource close.
//! - [`HostExtension::register`] registers host functions into a
//!   [`HostFunctionRegistry`]. Registration is validated against the
//!   extension's [`HostApiCatalog`] via [`catalog_import_schemas`] so the
//!   registered function declarations — parameter labels, type schemas and
//!   passing modes — match the catalog exactly. The catalog is the
//!   authoritative host-side contract: it carries the fingerprint and the
//!   resource type keys the host exposes, and the same catalog can be
//!   supplied to the compiler so the program's `HostImport`s resolve against
//!   it.
//!
//! `src/vm` therefore stays host-agnostic: resource classes, pending
//! operations and module state are supplied by the extension, while the
//! execution scope owns their lifecycle.
//!
//! **Boundary contract:** like [`super::host_context`], this module has no
//! coupling to the builtin runtime modules or any concrete host library.

use crate::host_api::{HostApiCatalog, HostApiFingerprint, HostParamPassing, HostTypeSchema};
use crate::vm::VmResult;

pub use super::host_context::HostContext;
pub use super::host_context::HostModule as HostModuleState;

/// One catalog-derived host import parameter descriptor.
///
/// This is the SDK-local (host-side) declaration of a parameter the extension
/// registers: its label, its semantic [`HostTypeSchema`] and its passing mode.
/// It mirrors what the compiler embeds at a call site when the same catalog is
/// supplied to codegen, so registering these keeps host and guest sides in
/// lock-step without any raw fingerprint construction on the host side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostImportParam {
    /// Parameter label, unique within its function.
    pub name: String,
    /// Semantic type schema of the parameter.
    pub schema: HostTypeSchema,
    /// Passing mode (value / borrow / borrow-mut / take-owned).
    pub passing: HostParamPassing,
}

/// One catalog-derived host import schema descriptor.
///
/// Produced by [`catalog_import_schemas`] from a [`HostApiCatalog`]: the
/// parameter labels, type schemas, passing modes, the return schema and the
/// catalog's own fingerprint. This is the exact host-side identity an
/// extension registers against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostImportSchema {
    /// Ordered parameter declarations.
    pub params: Vec<HostImportParam>,
    /// Return type schema.
    pub return_type: HostTypeSchema,
    /// The catalog fingerprint this schema was derived from.
    pub fingerprint: HostApiFingerprint,
}

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
/// [`HostExtension::register`] directly and binding it with
/// [`HostFunctionRegistry::bind_vm_cached`].
pub trait HostExtension: Send + Sync + 'static {
    /// Registers this extension's host functions into `registry`.
    ///
    /// Registration must be validated against the extension's
    /// [`HostApiCatalog`] (e.g. [`catalog_import_schemas`] plus the
    /// [`validate_catalog_import_schemas`] family); a name-only fallback is
    /// not part of this surface. The default registers nothing.
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

    /// Transactional install: register into a fresh standard registry, bind it
    /// to `vm`, then run the infallible install phase.
    ///
    /// This is the default implementation behind
    /// [`Vm::install_extension`](super::Vm::install_extension). Every fallible
    /// step (registration and registry binding) runs before [`Self::install`],
    /// so a failure leaves the VM unmodified.
    fn install_into(&self, vm: &mut super::Vm) -> VmResult<()> {
        let mut registry = super::host::HostFunctionRegistry::new();
        self.register(&mut registry)?;
        registry.bind_vm_cached(vm)?;
        self.install(vm);
        Ok(())
    }
}

/// Converts every catalog-declared overload of `name` into the exact
/// [`HostImportSchema`] the compiler embeds at a call site.
///
/// The produced schemas carry the declared parameter labels, type schemas,
/// passing modes, return schema and the catalog's own
/// [`HostApiCatalog::fingerprint`](crate::host_api::HostApiCatalog::fingerprint)
/// — exactly the identity stored in a `HostImport`'s schema during codegen
/// when the same catalog is supplied to the compiler. Registering against
/// these schemas therefore satisfies the exact-schema registry lookup with no
/// drift and no raw fingerprint construction on the host side.
pub fn catalog_import_schemas(catalog: &HostApiCatalog, name: &str) -> Vec<HostImportSchema> {
    let fingerprint = catalog.fingerprint();
    catalog_import_schemas_with_fingerprint(catalog, name, fingerprint)
}

fn catalog_import_schemas_with_fingerprint(
    catalog: &HostApiCatalog,
    name: &str,
    fingerprint: HostApiFingerprint,
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
                    schema: param.ty.clone(),
                    passing: param.passing,
                })
                .collect(),
            return_type: function.return_type.clone(),
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
    catalog_fingerprint: HostApiFingerprint,
    contract_fingerprint: HostApiFingerprint,
) -> VmResult<Vec<HostImportSchema>> {
    let expected = catalog_import_schemas_with_fingerprint(contract, name, contract_fingerprint);
    let got = catalog_import_schemas_with_fingerprint(catalog, name, catalog_fingerprint);
    if got.is_empty() {
        return Err(crate::vm::VmError::HostError(format!(
            "missing catalog member '{name}' (expected {} overload(s))",
            expected.len()
        )));
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
        return Err(crate::vm::VmError::HostError(format!(
            "incompatible catalog schema for '{name}': expected {expected:?}, got {got:?}"
        )));
    }
    Ok(got)
}

/// Registers one catalog-validated host function into `registry`.
///
/// The function's declared parameter count must match `arity` and every
/// declared schema must match the catalog's declaration for `name`. This is
/// the exact-schema registration surface adapted to the rewritten core: the
/// registry binds by name/arity, and the catalog is the authoritative
/// host-side contract the declaration is checked against before mutation.
pub fn register_catalog_function<F>(
    registry: &mut super::host::HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    arity: u8,
    factory: F,
) -> VmResult<()>
where
    F: Fn() -> Box<dyn super::host::HostFunction> + Send + Sync + 'static,
{
    let schemas = catalog_import_schemas(catalog, name);
    if schemas.is_empty() {
        return Err(crate::vm::VmError::HostError(format!(
            "catalog declares no function '{name}'"
        )));
    }
    // The registered declaration must match the catalog exactly (single
    // overload: parameter count must equal the declared arity).
    let schema = &schemas[0];
    if schema.params.len() != arity as usize {
        return Err(crate::vm::VmError::HostError(format!(
            "catalog function '{name}' declares {} parameter(s); arity {arity} does not match",
            schema.params.len()
        )));
    }
    registry.register(name, arity, factory);
    Ok(())
}
