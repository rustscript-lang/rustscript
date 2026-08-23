//! Generic contract for composing standard host surfaces.
//!
//! The host-agnostic VM core must not know which concrete standard domains
//! exist (`io::`, `http::`, `sqlite::`, …) or which same-crate builtin modules
//! implement them. All of that knowledge belongs to the standard builtin
//! composition layer. This module defines the *generic* abstraction the core
//! consumes instead:
//!
//! - [`StandardSurfaceMask`] — an opaque bit-mask naming which standard
//!   surfaces an import set requires / a registry already carries. The VM core
//!   treats it as an opaque token; only the composition implementation assigns
//!   meaning to individual bits.
//! - [`StandardSurfaceComposition`] — the caller-provided strategy the core
//!   delegates to for: deciding whether an exact import belongs to the
//!   standard catalog, computing which surfaces are required / already
//!   present, staging the missing surfaces onto a registry, building a fresh
//!   full-standard default registry, and binding a legacy by-name default host
//!   function.
//!
//! The standard builtin layer installs its concrete implementation once per
//! process via [`install_default_composition`]; [`default_composition`]
//! returns it. `src/vm` never names a concrete domain module or feature.
//!
//! This module is compiled only under `feature = "runtime"` (like the rest of
//! `src/vm`).

use std::sync::{Arc, OnceLock};

use crate::bytecode::HostImport;
use crate::host_api::HostApiFingerprint;

use super::host::HostFunctionRegistry;
use super::{Vm, VmResult};

/// Opaque bit-mask identifying a set of standard host surfaces.
///
/// The VM core only unions / intersects / tests-for-empty these masks and
/// hands them back to the composition implementation. It never interprets a
/// specific bit, so the concrete surface assignment (IO, HTTP, SQLite, …) can
/// change inside the composition layer without touching `src/vm`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandardSurfaceMask(u64);

impl StandardSurfaceMask {
    pub(crate) const fn empty() -> Self {
        Self(0)
    }

    /// Whether any surface is required.
    pub(crate) fn none(self) -> bool {
        self.0 == 0
    }

    /// Whether the surface at `bit` is set.
    pub(crate) fn contains(self, bit: u8) -> bool {
        self.0 & (1u64 << bit) != 0
    }

    /// Builds a mask with exactly the surface at `bit` set.
    pub(crate) fn from_bit(bit: u8) -> Self {
        Self(1u64 << bit)
    }

    /// Surfaces required but not already present.
    pub(crate) fn missing(self, present: Self) -> Self {
        Self(self.0 & !present.0)
    }

    /// Merges another mask into this one.
    pub(crate) fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Bit positions assigned by the standard builtin composition layer.
///
/// These are deliberately private to `src/vm`: the composition implementation
/// (in `builtins::runtime`) is the only producer/consumer of concrete bits.
/// The VM core passes masks around opaquely.
pub(crate) const SURFACE_BIT_IO: u8 = 0;
pub(crate) const SURFACE_BIT_HTTP: u8 = 1;
pub(crate) const SURFACE_BIT_DATABASE: u8 = 2;

/// Caller-provided strategy for composing the standard host surfaces.
///
/// Implemented by the standard builtin composition layer
/// (`crate::builtins::runtime`). The VM core invokes it generically and never
/// names a concrete domain module, namespace prefix, or feature.
pub trait StandardSurfaceComposition: Send + Sync {
    /// The authoritative catalog fingerprint of the composed standard catalog.
    fn standard_catalog_fingerprint(&self) -> HostApiFingerprint;

    /// Whether `import` belongs to the standard catalog (name resolves and
    /// the import's exact schema fingerprint matches the standard one).
    fn import_in_standard(&self, import: &HostImport) -> bool;

    /// Which standard surfaces the import set requires (opaque mask).
    fn required_surface_mask(&self, imports: &[HostImport]) -> StandardSurfaceMask;

    /// Which standard surfaces `registry` already carries (opaque mask).
    fn present_surface_mask(&self, registry: &HostFunctionRegistry) -> StandardSurfaceMask;

    /// Stages the surfaces named by `missing` onto `registry`.
    fn stage_missing(
        &self,
        registry: &mut HostFunctionRegistry,
        missing: StandardSurfaceMask,
    ) -> VmResult<()>;

    /// Builds a fresh registry carrying every enabled standard surface.
    fn build_default_registry(&self) -> VmResult<HostFunctionRegistry>;

    /// Binds the legacy by-name default host function `name` on `vm`, if one
    /// exists; returns whether it bound.
    fn bind_default_name(&self, vm: &mut Vm, name: &str) -> bool;
}

/// Process-wide default standard composition, installed by the standard
/// builtin layer. The VM core reads it lazily; a registry/VM constructed
/// before installation simply has no standard auto-stage/fallback (which is
/// correct: without the standard builtins there are no standard surfaces).
static DEFAULT_COMPOSITION: OnceLock<Arc<dyn StandardSurfaceComposition>> = OnceLock::new();

/// Installs the process-wide default standard composition.
///
/// Called by the standard builtin composition layer on first standard-catalog
/// access. Installing twice is a no-op (first wins).
pub(crate) fn install_default_composition(composition: Arc<dyn StandardSurfaceComposition>) {
    let _ = DEFAULT_COMPOSITION.set(composition);
}

/// Returns the installed default standard composition, if any.
pub(crate) fn default_composition() -> Option<Arc<dyn StandardSurfaceComposition>> {
    DEFAULT_COMPOSITION.get().cloned()
}
