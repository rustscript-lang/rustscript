//! Generic contract for composing standard host surfaces.
//!
//! The host-agnostic VM core must not know which concrete standard domains
//! exist (`io::`, `http::`, `sqlite::`, …) or which same-crate builtin modules
//! implement them. All of that knowledge belongs to the standard builtin
//! composition layer. This module defines the *generic* abstraction the core
//! consumes instead:
//!
//! - [`StandardSurfaceComposition`] — the caller-provided strategy the core
//!   delegates to for: deciding whether an exact import belongs to the
//!   standard catalog, ensuring the required standard surfaces are present on
//!   a registry (required/present/stage in one opaque call), building a fresh
//!   full-standard default registry, and binding a legacy by-name default host
//!   function.
//!
//! The composition is **explicit caller-provided per-instance state**: a
//! `HostFunctionRegistry` and a `Vm` carry an `Arc<dyn
//! StandardSurfaceComposition>` installed through the outer standard-runtime
//! constructor/registry path. There is deliberately no process-global slot and
//! no first-wins installation: `src/vm` never names a concrete domain module,
//! feature, surface count, or bit assignment.
//!
//! This module is compiled only under `feature = "runtime"` (like the rest of
//! `src/vm`).

use crate::bytecode::HostImport;
use crate::host_api::HostApiFingerprint;

use super::host::HostFunctionRegistry;
use super::{Vm, VmResult};

/// Caller-provided strategy for composing the standard host surfaces.
///
/// Implemented by the standard builtin composition layer
/// (`crate::builtins::runtime`). The VM core invokes it generically and never
/// names a concrete domain module, namespace prefix, feature, or surface
/// count.
pub trait StandardSurfaceComposition: Send + Sync {
    /// The authoritative catalog fingerprint of the composed standard catalog.
    fn standard_catalog_fingerprint(&self) -> HostApiFingerprint;

    /// Whether `import` belongs to the standard catalog (name resolves and
    /// the import's exact schema fingerprint matches the standard one).
    fn import_in_standard(&self, import: &HostImport) -> bool;

    /// Ensures every standard surface required by `imports` is present on
    /// `registry`, staging exactly the missing surfaces, and returns whether
    /// any surface was staged.
    ///
    /// This is the single opaque required/present/stage operation: the
    /// composition implementation computes which surfaces the import set
    /// requires and which the registry already carries, and registers only
    /// the missing ones. The VM core never sees a surface mask, a concrete
    /// surface count, or a bit assignment.
    fn ensure_surfaces(
        &self,
        imports: &[HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> VmResult<bool>;

    /// Builds a fresh registry carrying every enabled standard surface.
    fn build_default_registry(&self) -> VmResult<HostFunctionRegistry>;

    /// Binds the legacy by-name default host function `name` on `vm`, if one
    /// exists; returns whether it bound.
    fn bind_default_name(&self, vm: &mut Vm, name: &str) -> bool;
}
