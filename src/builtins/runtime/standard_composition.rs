//! Concrete standard-surface composition for the host-agnostic VM core.
//!
//! This module implements [`StandardSurfaceComposition`] for the same-crate
//! standard builtin layer. It is the *only* place that knows which concrete
//! standard domains exist (`io::`, `http::`, `sqlite::`) and which builtin
//! modules implement them. `src/vm` consumes it through the generic trait and
//! never names a domain, namespace prefix, or feature.
//!
//! The implementation is *caller-provided per-instance state*: the outer
//! standard-runtime constructor installs one instance on the standard
//! `HostFunctionRegistry` (and on the `Vm` for the legacy fallback paths)
//! through [`standard_composition`]. There is no process-global slot and no
//! hidden installation from `HostRuntime::new()`.

use std::sync::Arc;

use crate::bytecode::HostImport;
use crate::vm::standard_composition::StandardSurfaceComposition;
use crate::vm::{HostFunctionRegistry, Vm, VmResult};

use super::{
    StandardSurfaces, stage_missing_standard_surfaces, standard_exact_surface_requirements,
    standard_host_catalog, standard_host_catalog_fingerprint,
};

/// The concrete standard-surface composition for this build.
///
/// Feature-gated composition happens through the existing standard builtin
/// helpers: IO is always present under `runtime`, HTTP under `http-client`,
/// SQLite under `sqlite`. Required/present/stage is one opaque operation;
/// the VM core never sees a surface mask or count.
#[derive(Debug)]
pub(crate) struct StandardSurfaceCompositionImpl;

impl StandardSurfaceComposition for StandardSurfaceCompositionImpl {
    fn standard_catalog_fingerprint(&self) -> crate::host_api::HostApiFingerprint {
        standard_host_catalog_fingerprint()
    }

    fn import_in_standard(&self, import: &HostImport) -> bool {
        let Some(schema) = import.schema.as_ref() else {
            return false;
        };
        schema.fingerprint == standard_host_catalog_fingerprint()
            && !standard_host_catalog()
                .functions_named(&import.name)
                .is_empty()
    }

    fn ensure_surfaces(
        &self,
        imports: &[HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> VmResult<bool> {
        let (io, http, database) = standard_exact_surface_requirements(imports);
        let fingerprint = standard_host_catalog_fingerprint();
        // Present-surface computation: which standard surfaces does `registry`
        // already carry? Only standard-fingerprint exact entries count.
        let mut present = StandardSurfaces::default();
        for (name, fingerprints) in registry.exact_entries() {
            if !fingerprints.contains(&fingerprint) {
                continue;
            }
            if name.starts_with("io::") {
                present.io = true;
            } else if name.starts_with("http::") {
                present.http = true;
            } else if name.starts_with("sqlite::") {
                present.database = true;
            }
        }
        let missing = StandardSurfaces {
            io: io && !present.io,
            http: http && !present.http,
            database: database && !present.database,
        };
        if missing == StandardSurfaces::default() {
            return Ok(false);
        }
        stage_missing_standard_surfaces(registry, missing)?;
        Ok(true)
    }

    fn build_default_registry(&self) -> VmResult<HostFunctionRegistry> {
        super::standard_host_registry()
    }

    fn bind_default_name(&self, vm: &mut Vm, name: &str) -> bool {
        super::bind_default_host_function(vm, name)
    }
}

/// Returns a fresh concrete standard-surface composition instance.
///
/// The outer standard-runtime constructor installs this on the standard
/// registry and on a `Vm` when it wants default standard composition
/// behavior. Each call returns a new instance; there is no shared global.
pub fn standard_composition() -> Arc<dyn StandardSurfaceComposition> {
    Arc::new(StandardSurfaceCompositionImpl)
}
