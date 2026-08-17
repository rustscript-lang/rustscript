//! Generic host boundary: typed per-VM module state and host-agnostic
//! registration ports.
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
//! # Resource / operation registration ports
//!
//! The typed [`HostResourceRegistry`] and [`HostOperationRegistry`] traits are
//! the generic ports through which resource/operation registration is exposed.
//! The concrete storage — a `ResourceTable` and an `OperationRegistry` bound
//! per *execution scope* rather than per VM run budget — is owned by the
//! sibling `resource-table` / `operation-driver` integration scopes. Those
//! scopes implement these ports so this boundary can adapt without `src/vm`
//! importing a builtin domain module. The opaque tokens ([`HostHandle`],
//! [`HostKind`], [`HostOperationHandle`]) live here so the boundary stays
//! generic while the storage stays downstream.
//!
//! Host module state, by contrast, is owned directly by [`HostRuntime`]:
//! typed, per-VM, and deliberately **not** cleared on
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

    /// The stable non-domain namespace of this error (e.g. `"host::handle"`).
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
/// public surface. Resource / operation registration is exposed separately
/// through the port traits defined below, supplied per execution scope.
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

/// A positive, opaque handle to a typed host resource registered through the
/// boundary.
///
/// Owned by `src/vm` so the resource/operation boundary need not reference any
/// builtin type. The concrete `ResourceTable` that mints and resolves these is
/// supplied by the sibling `resource-table` integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostHandle(u64);

impl HostHandle {
    /// The raw token value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Decodes a raw token, rejecting the reserved zero handle.
    pub fn from_raw(raw: u64) -> HostContextResult<Self> {
        if raw == 0 {
            Err(HostContextError::new(
                "host::handle",
                "resource handle token must be non-zero",
            ))
        } else {
            Ok(Self(raw))
        }
    }
}

/// An immutable identity for one kind of typed host resource.
///
/// This is a generic token owned by the boundary; it is unrelated to any
/// domain-specific resource id and may be mapped by the resource-table
/// integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostKind(u16);

impl HostKind {
    /// Builds a kind token; the zero kind is reserved as invalid.
    pub const fn new(raw: u16) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    /// Builds a kind token without validating that it is non-zero.
    pub const fn new_unchecked(raw: u16) -> Self {
        Self(raw)
    }

    /// The raw kind identity.
    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// A typed value a host extension registers as a resource.
///
/// Implemented outside `src/vm` (by the resource-table integration or a host
/// embedding); never by a builtin domain module that `src/vm` may not import.
pub trait HostResource: Any + Send + 'static {
    /// The kind of this resource.
    const KIND: HostKind;
}

/// Registration port for typed host resources.
///
/// A concrete `ResourceTable` (sibling `resource-table` scope, owned per
/// execution scope) implements the erased-based port below and gains the typed
/// convenience methods for free. `HostContext` and this port never reference a
/// builtin domain module, so integration adapts without importing builtins.
///
/// # Integration adapter needs
///
/// The real adapter mints a [`HostHandle`] encoding arena/slot/generation and a
/// [`HostKind`]; mints close/borrow; and must be `Send` and per-execution-scope.
pub trait HostResourceRegistry: Send {
    /// Inserts an erased typed resource, returning a handle.
    fn insert_value(&mut self, value: Box<dyn Any + Send>) -> HostContextResult<HostHandle>;

    /// Inserts a typed resource with a cleanup closure invoked on close.
    fn insert_value_with_cleanup(
        &mut self,
        value: Box<dyn Any + Send>,
        cleanup: Box<dyn FnOnce() + Send>,
    ) -> HostContextResult<HostHandle>;

    /// Borrows an inserted resource by handle.
    fn borrow_value(&self, handle: HostHandle) -> HostContextResult<&(dyn Any + Send)>;

    /// Borrows an inserted resource mutably by handle.
    fn borrow_value_mut(&mut self, handle: HostHandle) -> HostContextResult<&mut (dyn Any + Send)>;

    /// Closes (and cleans up) an inserted resource by handle.
    fn close(&mut self, handle: HostHandle) -> HostContextResult<()>;

    /// Typed convenience: inserts a host resource value.
    fn insert<R: HostResource>(&mut self, value: R) -> HostContextResult<HostHandle> {
        self.insert_value(Box::new(value))
    }

    /// Typed convenience: borrows a host resource value.
    fn borrow<R: HostResource>(&self, handle: HostHandle) -> HostContextResult<&R> {
        self.borrow_value(handle)?
            .downcast_ref::<R>()
            .ok_or_else(|| {
                HostContextError::new("host::resource", "registered resource type mismatch")
            })
    }

    /// Typed convenience: borrows a host resource value mutably.
    fn borrow_mut<R: HostResource>(&mut self, handle: HostHandle) -> HostContextResult<&mut R> {
        self.borrow_value_mut(handle)?
            .downcast_mut::<R>()
            .ok_or_else(|| {
                HostContextError::new("host::resource", "registered resource type mismatch")
            })
    }
}

/// A positive, opaque handle to a submitted host operation registered through
/// the boundary.
///
/// Owned by `src/vm`; mirrored by the sibling `operation-driver` scope's
/// registry. The zero token is reserved and rejected on decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostOperationHandle(u64);

impl HostOperationHandle {
    /// The raw token value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Decodes a handle, rejecting the reserved zero value.
    pub fn from_raw(raw: u64) -> HostContextResult<Self> {
        if raw == 0 {
            Err(HostContextError::new(
                "host::operation",
                "operation handle token must be non-zero",
            ))
        } else {
            Ok(Self(raw))
        }
    }
}

/// Registration port for host operations.
///
/// A concrete `OperationRegistry` (sibling `operation-driver` scope, owned per
/// execution scope) implements submit/cancel so the host boundary can register
/// and cancel operations without importing any builtin domain module.
///
/// # Integration adapter expected
///
/// The real adapter maps a submitted [`HostOperation`] to an owned operation
/// core, returns a fresh [`HostOperationHandle`], and translates a `reason`
/// namespace into its internal cancellation cause.
pub trait HostOperationRegistry: Send {
    /// Registers a submitted host operation and returns its handle.
    fn submit(
        &mut self,
        operation: Box<dyn HostOperation + Send>,
    ) -> HostContextResult<HostOperationHandle>;

    /// Cancels a pending host operation. Returns `true` if a pending
    /// operation was transitioned to cancelled.
    fn cancel(
        &mut self,
        handle: HostOperationHandle,
        reason: &'static str,
    ) -> HostContextResult<bool>;
}

/// The host-agnostic view of a submitted operation. Implementors run (or queue)
/// the async work; the concrete `OperationRegistry` owns lifecycle.
pub trait HostOperation: Send {
    /// The immutable human-readable operation name for diagnostics.
    fn name(&self) -> &'static str;
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
