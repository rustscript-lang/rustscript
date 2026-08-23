//! Concrete standard-surface composition for the host-agnostic VM core.
//!
//! This module implements [`StandardSurfaceComposition`] for the same-crate
//! standard builtin layer. It is the *only* place that knows which concrete
//! standard domains exist (`io::`, `http::`, `sqlite::`) and which builtin
//! modules implement them. `src/vm` consumes it through the generic trait and
//! never names a domain, namespace prefix, or feature.
//!
//! The implementation is installed into the VM core's process-wide default
//! composition slot on first standard-catalog access (see
//! [`crate::vm::standard_composition::install_default_composition`]).

use std::sync::Arc;

use crate::bytecode::HostImport;
use crate::vm::standard_composition::{
    SURFACE_BIT_DATABASE, SURFACE_BIT_HTTP, SURFACE_BIT_IO, StandardSurfaceComposition,
    StandardSurfaceMask,
};
use crate::vm::{HostFunctionRegistry, Vm, VmResult};

use super::{
    StandardSurfaces, stage_missing_standard_surfaces, standard_exact_surface_requirements,
    standard_host_catalog, standard_host_catalog_fingerprint,
};

/// The concrete standard-surface composition for this build.
///
/// Feature-gated composition happens through the existing standard builtin
/// helpers: IO is always present under `runtime`, HTTP under `http-client`,
/// SQLite under `sqlite`. The mask bits are private to this module; the VM
/// core only passes them around opaquely.
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

    fn required_surface_mask(&self, imports: &[HostImport]) -> StandardSurfaceMask {
        let (io, http, database) = standard_exact_surface_requirements(imports);
        let mut mask = StandardSurfaceMask::empty();
        if io {
            mask = mask.union(StandardSurfaceMask::from_bit(SURFACE_BIT_IO));
        }
        if http {
            mask = mask.union(StandardSurfaceMask::from_bit(SURFACE_BIT_HTTP));
        }
        if database {
            mask = mask.union(StandardSurfaceMask::from_bit(SURFACE_BIT_DATABASE));
        }
        mask
    }

    fn present_surface_mask(&self, registry: &HostFunctionRegistry) -> StandardSurfaceMask {
        let fingerprint = standard_host_catalog_fingerprint();
        let mut mask = StandardSurfaceMask::empty();
        for (name, fingerprints) in registry.exact_entries() {
            let is_standard = fingerprints.contains(&fingerprint);
            if !is_standard {
                continue;
            }
            if name.starts_with("io::") {
                mask = mask.union(StandardSurfaceMask::from_bit(SURFACE_BIT_IO));
            } else if name.starts_with("http::") {
                mask = mask.union(StandardSurfaceMask::from_bit(SURFACE_BIT_HTTP));
            } else if name.starts_with("sqlite::") {
                mask = mask.union(StandardSurfaceMask::from_bit(SURFACE_BIT_DATABASE));
            }
        }
        mask
    }

    fn stage_missing(
        &self,
        registry: &mut HostFunctionRegistry,
        missing: StandardSurfaceMask,
    ) -> VmResult<()> {
        let surfaces = StandardSurfaces {
            io: missing.contains(SURFACE_BIT_IO),
            http: missing.contains(SURFACE_BIT_HTTP),
            database: missing.contains(SURFACE_BIT_DATABASE),
        };
        stage_missing_standard_surfaces(registry, surfaces)
    }

    fn build_default_registry(&self) -> VmResult<HostFunctionRegistry> {
        super::standard_host_registry()
    }

    fn bind_default_name(&self, vm: &mut Vm, name: &str) -> bool {
        super::bind_default_host_function(vm, name)
    }
}

/// Lazily constructs and installs the concrete standard composition into the
/// VM core's process-wide default slot. Idempotent (first install wins).
pub(crate) fn ensure_standard_composition_installed() {
    use crate::vm::standard_composition::install_default_composition;
    install_default_composition(Arc::new(StandardSurfaceCompositionImpl));
}
