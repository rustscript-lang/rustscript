//! Generic host boundary: typed per-VM module state and a generic
//! host-agnostic execution-scope SDK.
//!
//! [`HostContext`] is the public, builtin-agnostic surface that a host
//! embedding or an external host extension (a module living outside
//! `src/builtins/**`) uses to register typed, per-VM module state, push typed
//! [`HostResource`]s (root or child), start [`HostOperation`]s, and read back
//! resources / operation status. Every scope SDK method delegates to the
//! [`ExecutionScope`] owned by the underlying
//! [`HostRuntime`](super::host_runtime::HostRuntime), so all inserts land in
//! the same live scope and a Closing/Quiescent scope rejects them with a
//! structured [`ExecutionScopeError::ScopeClosing`] (propagated through
//! [`HostContextErrorKind::Scope`]).
//!
//! It never hands out the underlying [`HostRuntime`](super::host_runtime::HostRuntime)
//! and never names a builtin domain module; concrete SQLite / IO / HTTP / SSE
//! remain same-crate builtins, but `src/vm` must not depend on any of their
//! implementation modules or on `rusqlite`.
//!
//! **Boundary contract (enforced by `tests/host_context_arch_tests.rs`):**
//! this module references neither `crate::builtins::*` nor `rusqlite`.
//!
//! Host module state is owned directly by [`HostRuntime`]: typed, per-VM, and
//! deliberately **not** cleared on
//! [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse) or on execution-scope
//! close. Registered state therefore survives invocation resets — and scope
//! recycling — for the lifetime of the VM.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::task::{Context, Poll};

use super::execution_scope::{ExecutionScope, ExecutionScopeError, ScopeCloseOutcome, ScopeState};
use super::host_runtime::HostRuntime;
use super::operation::{OperationError, OperationId, OperationSpec, OperationStatus};
use super::resource::{
    CloseProgress, HostResource, Resource, ResourceAccessFrame, ResourceAccessRequest,
    ResourceCloseReason, ResourceError, ResourceHandle, ResourceMut, ResourceOwnership,
    ResourceRef, ResourceTypeKey,
};

/// Marker bound for a typed chunk of per-VM host module state.
///
/// A host extension implements this for exactly one concrete `State` type and
/// registers it through [`HostContext::set_module_state`]. State is typed at
/// compile time (keyed by [`TypeId`]) and is per-`Vm`; it is intentionally not
/// cleared by [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse) or by
/// execution-scope close, so policy / extension configuration survives across
/// invocation resets.
pub trait HostModule: Any + Send + 'static {}

/// Blanket implementation so any `Send` value can be registered as typed
/// per-VM module state; the trait remains a documentation/constraint marker.
impl<T: Any + Send + 'static> HostModule for T {}

/// Structured failure kind carried by [`HostContextError`].
///
/// The generic boundary preserves the underlying structured error instead of
/// flattening it into a message, so callers can match machine-readably (e.g.
/// a rejected insert while the scope is Closing surfaces as
/// [`Self::Scope`]`(ExecutionScopeError::ScopeClosing)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostContextErrorKind {
    /// A plain boundary failure carrying only namespace + message.
    Generic,
    /// A structured failure from the execution scope (write / lifecycle
    /// path: insert rejection while Closing, shutdown sequencing).
    Scope(ExecutionScopeError),
    /// A structured failure from the resource layer (typed borrow / handle
    /// recovery).
    Resource(ResourceError),
    /// A structured failure from the operation layer (status query).
    Operation(OperationError),
}

/// Error surfaced by the generic host boundary.
///
/// Carries a stable, non-domain `namespace` plus a human-readable message so
/// host-agnostic failures can be surfaced without referencing any builtin
/// domain type, and a structured [`HostContextErrorKind`] so generic
/// lifecycle violations stay machine-matchable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostContextError {
    namespace: &'static str,
    message: String,
    kind: HostContextErrorKind,
}

impl HostContextError {
    /// Builds a boundary error with a stable (non-domain) namespace.
    pub fn new(namespace: &'static str, message: impl Into<String>) -> Self {
        Self {
            namespace,
            message: message.into(),
            kind: HostContextErrorKind::Generic,
        }
    }

    /// Builds a boundary error from a structured execution-scope failure.
    fn from_scope(error: ExecutionScopeError) -> Self {
        let message = error.to_string();
        Self {
            namespace: "host::scope",
            message,
            kind: HostContextErrorKind::Scope(error),
        }
    }

    /// Builds a boundary error from a structured resource-layer failure.
    fn from_resource(error: ResourceError) -> Self {
        let message = error.to_string();
        Self {
            namespace: "host::resource",
            message,
            kind: HostContextErrorKind::Resource(error),
        }
    }

    /// Builds a boundary error from a structured operation-layer failure.
    fn from_operation(error: OperationError) -> Self {
        let message = error.to_string();
        Self {
            namespace: "host::operation",
            message,
            kind: HostContextErrorKind::Operation(error),
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

    /// The structured failure kind of this error.
    pub fn kind(&self) -> &HostContextErrorKind {
        &self.kind
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
/// so external host extensions can register typed per-VM state and drive the
/// generic execution scope through a stable public surface.
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

    // ---- generic execution-scope SDK ---------------------------------------

    /// Read-only access to the execution scope owned by this VM's host
    /// runtime (observe lifecycle state, resource/operation counts, typed
    /// borrows and status).
    ///
    /// The scope is never handed out mutably through the generic boundary: all
    /// mutations flow through the guarded SDK methods below.
    pub fn execution_scope(&self) -> &ExecutionScope {
        self.host.execution_scope()
    }

    /// The current lifecycle phase of this VM's execution scope.
    pub fn scope_state(&self) -> ScopeState {
        self.host.execution_scope_state()
    }

    /// Whether the execution scope is still accepting resource / operation
    /// inserts.
    pub fn is_scope_active(&self) -> bool {
        self.host.execution_scope_is_active()
    }

    /// Whether the execution scope reached terminal quiescence.
    pub fn is_scope_quiescent(&self) -> bool {
        self.host.execution_scope_is_quiescent()
    }

    /// Number of live resources in the current execution scope.
    pub fn resource_count(&self) -> usize {
        self.host.execution_scope_resource_count()
    }

    /// Number of occupied operation slots in the current execution scope.
    pub fn operation_count(&self) -> usize {
        self.host.execution_scope_operation_count()
    }

    /// Inserts a typed [`HostResource`] into the current execution scope,
    /// returning its typed capability token.
    ///
    /// A Closing/Quiescent scope rejects the insert with a structured
    /// [`HostContextErrorKind::Scope`]`(`[`ExecutionScopeError::ScopeClosing`]`)`.
    pub fn push_resource<T: HostResource>(&mut self, value: T) -> HostContextResult<Resource<T>> {
        self.host
            .execution_scope_push_resource(value)
            .map_err(HostContextError::from_scope)
    }

    /// Alias for [`Self::push_resource`], matching the public extension SDK
    /// naming for inserting a typed [`HostResource`] into the current scope.
    pub fn insert_resource<T: HostResource>(&mut self, value: T) -> HostContextResult<Resource<T>> {
        self.push_resource(value)
    }

    /// Inserts a resource using the exact catalog declaration key.
    pub fn push_resource_with_key<T: HostResource>(
        &mut self,
        value: T,
        key: crate::host_api::ResourceTypeKey,
    ) -> HostContextResult<Resource<T>> {
        self.host
            .execution_scope_push_resource_with_key(value, key)
            .map_err(HostContextError::from_scope)
    }

    /// Alias for [`Self::push_resource_with_key`], matching the public
    /// extension SDK naming for inserting a keyed resource.
    pub fn insert_resource_with_key<T: HostResource>(
        &mut self,
        value: T,
        key: crate::host_api::ResourceTypeKey,
    ) -> HostContextResult<Resource<T>> {
        self.push_resource_with_key(value, key)
    }

    /// Inserts a typed child resource linked to `parent`, so the parent cannot
    /// close before its children.
    pub fn push_child_resource<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
    ) -> HostContextResult<Resource<T>> {
        self.host
            .execution_scope_push_child_resource(value, parent)
            .map_err(HostContextError::from_scope)
    }

    /// Inserts a typed child resource under an explicit catalog key.
    pub fn push_child_resource_with_key<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
        key: ResourceTypeKey,
    ) -> HostContextResult<Resource<T>> {
        self.host
            .execution_scope_push_child_resource_with_key(value, parent, key)
            .map_err(HostContextError::from_scope)
    }

    /// Starts a host operation in the current execution scope from a full
    /// generic [`OperationSpec`] (driver, optional resource association,
    /// optional deadline, optional cleanup/cancel).
    pub fn start_operation(&mut self, spec: OperationSpec) -> HostContextResult<OperationId> {
        self.host
            .execution_scope_start_operation(spec)
            .map_err(HostContextError::from_scope)
    }

    /// Closes one resource in the current execution scope, first cancelling
    /// every operation associated with it (generic association logic — the
    /// core never dispatches on the concrete resource class).
    ///
    /// Maps `reason` onto the parallel operation-cancellation vocabulary
    /// before cancelling, then launches the resource's generic close via
    /// [`HostResource::begin_close`]. A `Pending` close is driven to
    /// completion by the usual scope poll machinery.
    pub fn close_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        reason: ResourceCloseReason,
    ) -> HostContextResult<CloseProgress> {
        self.host
            .execution_scope_close_resource::<T>(handle, reason)
            .map_err(HostContextError::from_scope)
    }

    /// Immutably borrows a live resource for the duration of a host call.
    ///
    /// The token is re-validated against the current scope (arena, slot
    /// generation, `TypeId`, open state); a stale / wrong-type / foreign-scope
    /// token fails with a structured resource-layer error.
    pub fn resource<T: HostResource>(
        &self,
        token: &Resource<T>,
    ) -> HostContextResult<ResourceRef<'_, T>> {
        self.host
            .execution_scope()
            .resources()
            .get(token)
            .map_err(HostContextError::from_resource)
    }

    /// Mutably borrows a typed resource for the duration of this synchronous
    /// host call. The access frame validates the key and aliases before the
    /// mutable reference is created.
    pub fn resource_mut<T: HostResource>(
        &mut self,
        token: &Resource<T>,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        let request = ResourceAccessRequest::borrow_mut::<T>(token.handle());
        let frame = self
            .host
            .execution_scope_begin_resource_access(vec![request])
            .map_err(HostContextError::from_scope)?;
        frame.borrow_mut(0).map_err(HostContextError::from_resource)
    }

    /// Mutably borrows a legacy resource whose exact key is supplied by the
    /// caller. Static keyed resources are checked against the supplied key.
    pub fn resource_mut_with_key<T: HostResource>(
        &mut self,
        token: &Resource<T>,
        key: ResourceTypeKey,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        let request = ResourceAccessRequest::borrow_mut_with_key::<T>(token.handle(), key);
        let frame = self
            .host
            .execution_scope_begin_resource_access(vec![request])
            .map_err(HostContextError::from_scope)?;
        frame.borrow_mut(0).map_err(HostContextError::from_resource)
    }

    /// Starts a multi-argument resource frame. All raw handles, concrete
    /// `TypeId`s, declaration keys, ownership states, child links, associated
    /// operations, and same-handle aliases are checked before any take.
    pub fn begin_resource_access(
        &mut self,
        requests: Vec<ResourceAccessRequest>,
    ) -> HostContextResult<ResourceAccessFrame<'_>> {
        self.host
            .execution_scope_begin_resource_access(requests)
            .map_err(HostContextError::from_scope)
    }

    /// Validates a raw [`ResourceHandle`] against the current scope and
    /// recovers a typed token (read-only).
    pub fn typed_resource<T: HostResource>(
        &self,
        handle: ResourceHandle,
    ) -> HostContextResult<Resource<T>> {
        self.host
            .execution_scope()
            .resources()
            .typed(handle)
            .map_err(HostContextError::from_resource)
    }

    /// Borrow a raw handle after typed arena/generation/key validation.
    pub fn borrow_resource<T: HostResource>(
        &self,
        handle: ResourceHandle,
    ) -> HostContextResult<ResourceRef<'_, T>> {
        let token = self.typed_resource::<T>(handle)?;
        self.resource(&token)
    }

    /// Mutably borrow a raw handle after typed arena/generation/key validation.
    pub fn borrow_resource_mut<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        let request = ResourceAccessRequest::borrow_mut::<T>(handle);
        let frame = self
            .host
            .execution_scope_begin_resource_access(vec![request])
            .map_err(HostContextError::from_scope)?;
        frame.borrow_mut(0).map_err(HostContextError::from_resource)
    }

    /// Mutably borrows a raw legacy handle with an explicit exact key.
    pub fn borrow_resource_mut_with_key<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        key: ResourceTypeKey,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        let request = ResourceAccessRequest::borrow_mut_with_key::<T>(handle, key);
        let frame = self
            .host
            .execution_scope_begin_resource_access(vec![request])
            .map_err(HostContextError::from_scope)?;
        frame.borrow_mut(0).map_err(HostContextError::from_resource)
    }

    /// Atomically takes a guest-owned raw handle using its concrete type and
    /// declaration key.
    pub fn take_owned<T: HostResource>(&mut self, handle: ResourceHandle) -> HostContextResult<T> {
        self.take_resource::<T>(handle)
    }

    /// Atomically takes a guest-owned resource out of the current scope,
    /// transferring ownership of the concrete value to the caller. See
    /// [`ExecutionScope::take_resource`] for the validation contract.
    pub fn take_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
    ) -> HostContextResult<T> {
        self.host
            .execution_scope_take_resource::<T>(handle)
            .map_err(HostContextError::from_scope)
    }

    /// Atomically takes a legacy resource with an explicit exact key through
    /// the operation-aware access frame.
    pub fn take_resource_with_key<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        key: ResourceTypeKey,
    ) -> HostContextResult<T> {
        self.host
            .execution_scope_take_resource_with_key::<T>(handle, key)
            .map_err(HostContextError::from_scope)
    }

    /// Marks an open, host-owned resource as guest-owned in the current
    /// scope (ownership transfer from the host to the guest script). See
    /// [`ExecutionScope::mark_resource_guest_owned`] for the atomic
    /// validation contract.
    pub fn mark_resource_guest_owned(&mut self, handle: ResourceHandle) -> HostContextResult<()> {
        self.host
            .execution_scope_mark_guest_owned(handle)
            .map_err(HostContextError::from_scope)
    }

    /// The current ownership state of the resource `handle` names, or `None`
    /// when the handle is foreign or stale in this scope.
    pub fn resource_ownership(&self, handle: ResourceHandle) -> Option<ResourceOwnership> {
        self.host.execution_scope().resources().ownership(handle)
    }

    /// Observes the current status of a host operation in the current scope.
    pub fn operation_status(&self, id: OperationId) -> HostContextResult<OperationStatus> {
        self.host
            .execution_scope()
            .operations()
            .status(id)
            .map_err(HostContextError::from_operation)
    }

    /// Begins shutdown of the current execution scope (**Active → Closing**),
    /// sealing new resource/operation inserts.
    ///
    /// Idempotent and first-reason-wins, mirroring the underlying scope.
    pub fn begin_close(&mut self, reason: ResourceCloseReason) -> HostContextResult<bool> {
        self.host
            .execution_scope_begin_close(reason)
            .map_err(HostContextError::from_scope)
    }

    /// Drives the closing scope to quiescence with the caller's context.
    ///
    /// Returns `Poll::Pending` while any operation or resource is still
    /// pending, and `Poll::Ready` with the terminal outcome once both the
    /// operation registry and the resource table are empty. Read-only queries
    /// remain available while closing.
    pub fn poll_close(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<HostContextResult<ScopeCloseOutcome>> {
        match self.host.execution_scope_poll_close(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.map_err(HostContextError::from_scope)),
        }
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
