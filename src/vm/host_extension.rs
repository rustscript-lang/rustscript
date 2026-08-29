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

use crate::host_api::{
    HostApiCatalog, HostApiFingerprint, HostFunctionSchema, HostParamPassing, HostTypeSchema,
};
use crate::vm::VmResult;

pub use super::host_context::HostContext;
pub use super::host_context::HostModule as HostModuleState;
pub use crate::host_api::{HostImportParam, HostImportSchema};

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
            name: function.name.clone(),
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

/// A structured failure from catalog-backed host-function registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogRegistrationError {
    /// The catalog has no declaration for the requested function name.
    MissingFunction { name: String },
    /// The selected schema was produced from a different catalog.
    FingerprintMismatch {
        name: String,
        expected: HostApiFingerprint,
        actual: HostApiFingerprint,
    },
    /// The selected declaration has a different number of parameters.
    ArityMismatch {
        name: String,
        expected: usize,
        actual: usize,
    },
    /// A parameter's semantic type differs from the catalog declaration.
    ParameterTypeMismatch {
        name: String,
        index: usize,
        expected: HostTypeSchema,
        actual: HostTypeSchema,
    },
    /// A parameter's passing mode differs from the catalog declaration.
    ParameterPassingMismatch {
        name: String,
        index: usize,
        expected: HostParamPassing,
        actual: HostParamPassing,
    },
    /// A parameter label differs from the catalog declaration.
    ParameterNameMismatch {
        name: String,
        index: usize,
        expected: String,
        actual: String,
    },
    /// The selected declaration has a different return schema.
    ReturnTypeMismatch {
        name: String,
        expected: HostTypeSchema,
        actual: HostTypeSchema,
    },
    /// More than one catalog overload matches an arity-only selection.
    AmbiguousOverload {
        name: String,
        arity: usize,
        candidates: usize,
    },
    /// The selection was not equal to any catalog declaration, but no more
    /// specific field-level mismatch could be reported.
    SchemaMismatch {
        name: String,
        selected: Box<HostImportSchema>,
        candidates: Box<Vec<HostImportSchema>>,
    },
    /// The full schema was valid for the catalog but already occupied a
    /// registry slot, or would be ambiguous with an existing call shape.
    RegistryConflict { name: String, detail: String },
    /// The caller supplied a schema whose bounded representation is invalid.
    InvalidSchema { name: String, detail: String },
}

impl std::fmt::Display for CatalogRegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingFunction { name } => {
                write!(f, "catalog declares no function '{name}'")
            }
            Self::FingerprintMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "catalog fingerprint mismatch for '{name}': expected {expected}, got {actual}"
            ),
            Self::ArityMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "catalog arity mismatch for '{name}': expected {expected}, got {actual}"
            ),
            Self::ParameterTypeMismatch {
                name,
                index,
                expected,
                actual,
            } => write!(
                f,
                "catalog parameter {index} type mismatch for '{name}': expected {expected:?}, got {actual:?}"
            ),
            Self::ParameterPassingMismatch {
                name,
                index,
                expected,
                actual,
            } => write!(
                f,
                "catalog parameter {index} passing mismatch for '{name}': expected {expected:?}, got {actual:?}"
            ),
            Self::ParameterNameMismatch {
                name,
                index,
                expected,
                actual,
            } => write!(
                f,
                "catalog parameter {index} name mismatch for '{name}': expected '{expected}', got '{actual}'"
            ),
            Self::ReturnTypeMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "catalog return type mismatch for '{name}': expected {expected:?}, got {actual:?}"
            ),
            Self::AmbiguousOverload {
                name,
                arity,
                candidates,
            } => write!(
                f,
                "catalog function '{name}' has {candidates} overloads with arity {arity}; select a full schema"
            ),
            Self::SchemaMismatch {
                name,
                selected,
                candidates,
            } => write!(
                f,
                "selected schema for '{name}' does not match any of {candidates:?}: {selected:?}"
            ),
            Self::RegistryConflict { name, detail } => {
                write!(f, "cannot register catalog function '{name}': {detail}")
            }
            Self::InvalidSchema { name, detail } => {
                write!(f, "invalid catalog schema for '{name}': {detail}")
            }
        }
    }
}

impl std::error::Error for CatalogRegistrationError {}

impl From<CatalogRegistrationError> for crate::vm::VmError {
    fn from(error: CatalogRegistrationError) -> Self {
        Self::HostError(error.to_string())
    }
}

/// Selects one full declaration from a catalog for registration.
///
/// Passing a [`HostImportSchema`] performs field-by-field exact selection.
/// Passing an integer retains the legacy arity-only surface for catalogs with a
/// single matching overload; it returns [`CatalogRegistrationError::AmbiguousOverload`]
/// whenever arity alone cannot identify one declaration.
pub trait CatalogSchemaSelection {
    fn select_schema(
        &self,
        catalog: &HostApiCatalog,
        name: &str,
    ) -> Result<HostImportSchema, CatalogRegistrationError>;
}

fn import_schema_from_function(
    catalog: &HostApiCatalog,
    function: &HostFunctionSchema,
) -> HostImportSchema {
    HostImportSchema {
        name: function.name.clone(),
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
        fingerprint: catalog.fingerprint(),
    }
}

impl CatalogSchemaSelection for HostFunctionSchema {
    fn select_schema(
        &self,
        catalog: &HostApiCatalog,
        name: &str,
    ) -> Result<HostImportSchema, CatalogRegistrationError> {
        if let Err(error) = self.validate() {
            return Err(CatalogRegistrationError::InvalidSchema {
                name: self.name.clone(),
                detail: error.to_string(),
            });
        }
        if self.name != name {
            return Err(CatalogRegistrationError::SchemaMismatch {
                name: name.to_string(),
                selected: Box::new(import_schema_from_function(catalog, self)),
                candidates: Box::new(catalog_import_schemas(catalog, name)),
            });
        }
        import_schema_from_function(catalog, self).select_schema(catalog, name)
    }
}

fn schema_field_mismatch(
    name: &str,
    selected: &HostImportSchema,
    candidate: &HostImportSchema,
) -> Option<CatalogRegistrationError> {
    for (index, (expected, actual)) in candidate
        .params
        .iter()
        .zip(selected.params.iter())
        .enumerate()
    {
        if expected.schema != actual.schema {
            return Some(CatalogRegistrationError::ParameterTypeMismatch {
                name: name.to_string(),
                index,
                expected: expected.schema.clone(),
                actual: actual.schema.clone(),
            });
        }
        if expected.passing != actual.passing {
            return Some(CatalogRegistrationError::ParameterPassingMismatch {
                name: name.to_string(),
                index,
                expected: expected.passing,
                actual: actual.passing,
            });
        }
        if expected.name != actual.name {
            return Some(CatalogRegistrationError::ParameterNameMismatch {
                name: name.to_string(),
                index,
                expected: expected.name.clone(),
                actual: actual.name.clone(),
            });
        }
    }
    if candidate.return_type != selected.return_type {
        return Some(CatalogRegistrationError::ReturnTypeMismatch {
            name: name.to_string(),
            expected: candidate.return_type.clone(),
            actual: selected.return_type.clone(),
        });
    }
    None
}

impl CatalogSchemaSelection for HostImportSchema {
    fn select_schema(
        &self,
        catalog: &HostApiCatalog,
        name: &str,
    ) -> Result<HostImportSchema, CatalogRegistrationError> {
        if let Err(error) = self.validate() {
            return Err(CatalogRegistrationError::InvalidSchema {
                name: self.name.clone(),
                detail: error.to_string(),
            });
        }
        let candidates = catalog_import_schemas(catalog, name);
        if candidates.is_empty() {
            return Err(CatalogRegistrationError::MissingFunction {
                name: name.to_string(),
            });
        }
        let expected_fingerprint = catalog.fingerprint();
        if self.fingerprint != expected_fingerprint {
            return Err(CatalogRegistrationError::FingerprintMismatch {
                name: name.to_string(),
                expected: expected_fingerprint,
                actual: self.fingerprint,
            });
        }
        let exact: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                candidate.params == self.params && candidate.return_type == self.return_type
            })
            .collect();
        if exact.len() == 1 {
            return Ok((*exact[0]).clone());
        }
        if exact.len() > 1 {
            return Err(CatalogRegistrationError::AmbiguousOverload {
                name: name.to_string(),
                arity: self.params.len(),
                candidates: exact.len(),
            });
        }
        let same_arity: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.params.len() == self.params.len())
            .collect();
        if same_arity.is_empty() {
            return Err(CatalogRegistrationError::ArityMismatch {
                name: name.to_string(),
                expected: candidates
                    .iter()
                    .map(|candidate| candidate.params.len())
                    .next()
                    .unwrap_or_default(),
                actual: self.params.len(),
            });
        }
        if same_arity.len() == 1 {
            let candidate = same_arity[0];
            return Err(
                schema_field_mismatch(name, self, candidate).unwrap_or_else(|| {
                    CatalogRegistrationError::SchemaMismatch {
                        name: name.to_string(),
                        selected: Box::new(self.clone()),
                        candidates: Box::new(candidates),
                    }
                }),
            );
        }
        Err(CatalogRegistrationError::SchemaMismatch {
            name: name.to_string(),
            selected: Box::new(self.clone()),
            candidates: Box::new(candidates),
        })
    }
}

impl CatalogSchemaSelection for u8 {
    fn select_schema(
        &self,
        catalog: &HostApiCatalog,
        name: &str,
    ) -> Result<HostImportSchema, CatalogRegistrationError> {
        usize::from(*self).select_schema(catalog, name)
    }
}

impl CatalogSchemaSelection for usize {
    fn select_schema(
        &self,
        catalog: &HostApiCatalog,
        name: &str,
    ) -> Result<HostImportSchema, CatalogRegistrationError> {
        let candidates = catalog_import_schemas(catalog, name);
        if candidates.is_empty() {
            return Err(CatalogRegistrationError::MissingFunction {
                name: name.to_string(),
            });
        }
        let matching: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.params.len() == *self)
            .collect();
        match matching.as_slice() {
            [] => Err(CatalogRegistrationError::ArityMismatch {
                name: name.to_string(),
                expected: candidates[0].params.len(),
                actual: *self,
            }),
            [schema] => Ok((*schema).clone()),
            _ => Err(CatalogRegistrationError::AmbiguousOverload {
                name: name.to_string(),
                arity: *self,
                candidates: matching.len(),
            }),
        }
    }
}

impl<T: CatalogSchemaSelection + ?Sized> CatalogSchemaSelection for &T {
    fn select_schema(
        &self,
        catalog: &HostApiCatalog,
        name: &str,
    ) -> Result<HostImportSchema, CatalogRegistrationError> {
        (*self).select_schema(catalog, name)
    }
}

fn selected_schema<S>(
    catalog: &HostApiCatalog,
    name: &str,
    selection: S,
) -> Result<HostImportSchema, CatalogRegistrationError>
where
    S: CatalogSchemaSelection,
{
    let schema = selection.select_schema(catalog, name)?;
    if schema.params.len() > usize::from(u8::MAX) {
        return Err(CatalogRegistrationError::ArityMismatch {
            name: name.to_string(),
            expected: usize::from(u8::MAX),
            actual: schema.params.len(),
        });
    }
    Ok(schema)
}

/// Registers one catalog-validated host function into `registry`.
///
/// The selected schema is checked for arity, parameter names/types/passing
/// modes, return type, and overload identity before the registry is mutated.
pub fn register_catalog_function<S, F>(
    registry: &mut super::host::HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    selection: S,
    factory: F,
) -> Result<(), CatalogRegistrationError>
where
    S: CatalogSchemaSelection,
    F: Fn() -> Box<dyn super::HostFunction> + Send + Sync + 'static,
{
    let schema = selected_schema(catalog, name, selection)?;
    registry
        .register_catalog(schema, factory)
        .map(|_| ())
        .map_err(|error| CatalogRegistrationError::RegistryConflict {
            name: name.to_string(),
            detail: error.to_string(),
        })
}

/// Static-function counterpart used by external extensions whose handlers are
/// function pointers rather than factory closures. It shares the exact same
/// schema selector and validation path.
pub fn register_catalog_static_function<S>(
    registry: &mut super::host::HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    selection: S,
    function: super::host::StaticHostFunction,
) -> Result<(), CatalogRegistrationError>
where
    S: CatalogSchemaSelection,
{
    let schema = selected_schema(catalog, name, selection)?;
    registry
        .register_catalog_static(schema, function)
        .map(|_| ())
        .map_err(|error| CatalogRegistrationError::RegistryConflict {
            name: name.to_string(),
            detail: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_api::{
        HostApiCatalog, HostApiFingerprint, HostFunctionSchema, HostParamSchema, HostTypeSchema,
    };
    use crate::vm::{CallOutcome, HostFunction, HostFunctionRegistry, Value, Vm};

    struct Noop;

    impl HostFunction for Noop {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(crate::vm::CallReturn::None))
        }
    }

    fn catalog_with_function(name: &str) -> HostApiCatalog {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            name,
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::String,
        ));
        builder.build().expect("test catalog should build")
    }

    fn register(
        registry: &mut HostFunctionRegistry,
        catalog: &HostApiCatalog,
        name: &str,
        schema: impl CatalogSchemaSelection,
    ) -> Result<(), CatalogRegistrationError> {
        register_catalog_function(registry, catalog, name, schema, || Box::new(Noop))
    }

    #[test]
    fn catalog_registration_accepts_the_selected_full_schema() {
        let catalog = catalog_with_function("demo::selected");
        let selected = catalog_import_schemas(&catalog, "demo::selected")
            .into_iter()
            .next()
            .expect("selected schema")
            .clone();
        let mut registry = HostFunctionRegistry::empty();

        register(&mut registry, &catalog, "demo::selected", selected)
            .expect("matching schema registers");
        assert!(registry.contains_name("demo::selected"));
    }

    #[test]
    fn catalog_registration_reports_each_full_schema_mismatch() {
        let catalog = catalog_with_function("demo::mismatch");
        let selected = catalog_import_schemas(&catalog, "demo::mismatch")
            .into_iter()
            .next()
            .expect("selected schema")
            .clone();

        let mut wrong_arity = selected.clone();
        wrong_arity.params.clear();
        let mut registry = HostFunctionRegistry::empty();
        assert!(matches!(
            register(&mut registry, &catalog, "demo::mismatch", wrong_arity),
            Err(CatalogRegistrationError::ArityMismatch { .. })
        ));

        let mut wrong_type = selected.clone();
        wrong_type.params[0].schema = HostTypeSchema::Bool;
        assert!(matches!(
            register(&mut registry, &catalog, "demo::mismatch", wrong_type),
            Err(CatalogRegistrationError::ParameterTypeMismatch { .. })
        ));

        let mut wrong_passing = selected.clone();
        wrong_passing.params[0].passing = HostParamPassing::Borrow;
        assert!(matches!(
            register(&mut registry, &catalog, "demo::mismatch", wrong_passing),
            Err(CatalogRegistrationError::ParameterPassingMismatch { .. })
        ));

        let mut wrong_return = selected.clone();
        wrong_return.return_type = HostTypeSchema::Bool;
        assert!(matches!(
            register(&mut registry, &catalog, "demo::mismatch", wrong_return),
            Err(CatalogRegistrationError::ReturnTypeMismatch { .. })
        ));

        let mut wrong_fingerprint = selected.clone();
        wrong_fingerprint.fingerprint =
            HostApiFingerprint::from_wire(wrong_fingerprint.fingerprint.as_u64() ^ 1);
        assert!(matches!(
            register(&mut registry, &catalog, "demo::mismatch", wrong_fingerprint),
            Err(CatalogRegistrationError::FingerprintMismatch { .. })
        ));
    }

    #[test]
    fn arity_only_registration_rejects_ambiguous_overloads() {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "demo::overloaded",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "demo::overloaded",
            vec![HostParamSchema::value("value", HostTypeSchema::String)],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("distinct overloads should build");
        let mut registry = HostFunctionRegistry::empty();

        assert!(matches!(
            register(&mut registry, &catalog, "demo::overloaded", 1_u8),
            Err(CatalogRegistrationError::AmbiguousOverload { .. })
        ));
    }
}
