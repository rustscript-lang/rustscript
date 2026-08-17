//! Generic host boundary: typed per-VM module state.
//!
//! [`HostContext`] is the public, builtin-agnostic surface that a host
//! embedding or an external host extension (a module living outside
//! `src/builtins/**`) uses to register typed, per-VM module state. It never
//! hands out the underlying [`HostRuntime`](super::host_runtime::HostRuntime)
//! and never names a builtin domain module; concrete SQLite / IO / HTTP / SSE
//! remain same-crate builtins, but `src/vm` must not depend on any of their
//! implementation modules or on `rusqlite`.
//!
//! **Boundary contract (enforced by `tests/host_context_arch_tests.rs`):**
//! this module references neither `crate::builtins::*` nor `rusqlite`.
//!
//! Host module state is owned directly by [`HostRuntime`]: typed, per-VM, and
//! deliberately **not** cleared on
//! [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse). Registered state
//! therefore survives invocation resets for the lifetime of the VM.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

use super::host_runtime::HostRuntime;

/// Marker bound for a typed chunk of per-VM host module state.
///
/// A host extension implements this for exactly one concrete `State` type and
/// registers it through [`HostContext::set_module_state`]. State is typed at
/// compile time (keyed by [`TypeId`]) and is per-`Vm`; it is intentionally not
/// cleared by [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse), so
/// policy / extension configuration survives across invocation resets.
pub trait HostModule: Any + Send + 'static {}

/// Blanket implementation so any `Send` value can be registered as typed
/// per-VM module state; the trait remains a documentation/constraint marker.
impl<T: Any + Send + 'static> HostModule for T {}

/// Error surfaced by the generic host boundary.
///
/// Carries a stable, non-domain `namespace` plus a human-readable message so
/// host-agnostic failures can be surfaced without referencing any builtin
/// domain type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostContextError {
    namespace: &'static str,
    message: String,
}

impl HostContextError {
    /// Builds a boundary error with a stable (non-domain) namespace.
    pub fn new(namespace: &'static str, message: impl Into<String>) -> Self {
        Self {
            namespace,
            message: message.into(),
        }
    }

    /// The stable non-domain namespace of this error (e.g. `"host::module"`).
    pub fn namespace(&self) -> &'static str {
        self.namespace
    }

    /// The human readable error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HostContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.namespace, self.message)
    }
}

impl std::error::Error for HostContextError {}

/// Result type used by the generic host boundary.
pub type HostContextResult<T> = Result<T, HostContextError>;

/// The public, generic host boundary for one [`Vm`](super::Vm).
///
/// Obtained from [`Vm::host_context`](super::Vm::host_context). It never leaks
/// the underlying [`HostRuntime`] and never references a builtin domain module,
/// so external host extensions can register typed per-VM state through a stable
/// public surface.
pub struct HostContext<'a> {
    host: &'a mut HostRuntime,
}

impl<'a> HostContext<'a> {
    pub(crate) fn new(host: &'a mut HostRuntime) -> Self {
        Self { host }
    }

    /// Registers typed per-VM module state, replacing any earlier value of the
    /// same type.
    ///
    /// Returns `true` when a previously registered value of the same type was
    /// replaced, and `false` when this value was freshly registered.
    pub fn set_module_state<M: HostModule>(&mut self, state: M) -> bool {
        self.host.set_module_state(state)
    }

    /// Borrows the registered typed module state, if any.
    pub fn module_state<M: HostModule>(&self) -> Option<&M> {
        self.host.get_module_state()
    }

    /// Borrows the registered typed module state mutably, if any.
    pub fn module_state_mut<M: HostModule>(&mut self) -> Option<&mut M> {
        self.host.get_module_state_mut()
    }

    /// Removes and returns the registered typed module state, if any.
    pub fn take_module_state<M: HostModule>(&mut self) -> Option<M> {
        self.host.remove_module_state()
    }

    /// Returns `true` when no module state is currently registered.
    pub fn is_module_state_empty(&self) -> bool {
        self.host.is_module_state_empty()
    }
}

/// A thin, typed dictionary of per-VM host module state used internally by
/// [`HostRuntime`].
///
/// Kept as a distinct type so the boundary's storage concerns (typed keying,
/// persistence across reset) stay separable from the host runtime's capability
/// and resource fields. Backed by a type-erased `HashMap<TypeId, Box<dyn Any>>`.
#[derive(Default, Debug)]
pub(crate) struct HostModuleStore {
    entries: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl HostModuleStore {
    /// Creates an empty module-state store.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers typed state, returning `true` if a value of the same type was
    /// replaced.
    pub(crate) fn set<M: HostModule>(&mut self, state: M) -> bool {
        let replaced = self.entries.contains_key(&TypeId::of::<M>());
        self.entries.insert(TypeId::of::<M>(), Box::new(state));
        replaced
    }

    /// Borrows typed state, if present.
    pub(crate) fn get<M: HostModule>(&self) -> Option<&M> {
        self.entries.get(&TypeId::of::<M>())?.downcast_ref()
    }

    /// Borrows typed state mutably, if present.
    pub(crate) fn get_mut<M: HostModule>(&mut self) -> Option<&mut M> {
        self.entries.get_mut(&TypeId::of::<M>())?.downcast_mut()
    }

    /// Removes and returns typed state, if present.
    pub(crate) fn take<M: HostModule>(&mut self) -> Option<M> {
        self.entries
            .remove(&TypeId::of::<M>())?
            .downcast::<M>()
            .ok()
            .map(|boxed| *boxed)
    }

    /// Returns whether the store holds no state.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
