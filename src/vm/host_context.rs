//! Generic host boundary: typed per-VM module state and the generic
//! host-agnostic execution-scope SDK.
//!
//! [`HostContext`] is the public, builtin-agnostic surface that a host
//! embedding or an external host extension (a module living outside
//! `src/builtins/**`) uses to register typed, per-VM module state, push typed
//! [`HostResource`]s, start [`HostOperation`]s, and read back resources /
//! operation status. Every scope SDK method delegates to the
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
#![allow(clippy::result_large_err)]

use std::any::Any;
use std::fmt;
use std::task::{Context, Poll};

use crate::host_api::ResourceTypeKey;

use super::Vm;
use super::execution_scope::{ExecutionScope, ExecutionScopeError, ScopeState};
use super::host_runtime::HostRuntime;
use super::operation::{
    OperationCancelReason, OperationError, OperationId, OperationOutcome, OperationSpec,
    OperationStatus,
};
use super::resource::{
    CloseProgress, HostResource, Resource, ResourceCloseReason, ResourceError, ResourceHandle,
    ResourceMut, ResourceRef, ResourceTable,
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
    vm: &'a mut Vm,
}

impl<'a> HostContext<'a> {
    pub(crate) fn new(vm: &'a mut Vm) -> Self {
        Self { vm }
    }

    /// Registers typed per-VM module state, replacing any earlier value of the
    /// same type.
    ///
    /// Returns `true` when a previously registered value of the same type was
    /// replaced, and `false` when this value was freshly registered.
    pub fn set_module_state<M: HostModule>(&mut self, state: M) -> bool {
        self.vm.host.set_module_state(state)
    }

    /// Borrows the registered typed module state, if any.
    pub fn module_state<M: HostModule>(&self) -> Option<&M> {
        self.vm.host.get_module_state()
    }

    /// Borrows the registered typed module state mutably, if any.
    pub fn module_state_mut<M: HostModule>(&mut self) -> Option<&mut M> {
        self.vm.host.get_module_state_mut()
    }

    /// Removes and returns the registered typed module state, if any.
    pub fn take_module_state<M: HostModule>(&mut self) -> Option<M> {
        self.vm.host.remove_module_state()
    }

    /// Returns `true` when no module state is currently registered.
    pub fn is_module_state_empty(&self) -> bool {
        self.vm.host.is_module_state_empty()
    }

    // ---- generic execution-scope SDK ---------------------------------------

    /// Read-only access to the execution scope owned by this VM's host
    /// runtime (observe lifecycle state, resource/operation counts, typed
    /// borrows and status).
    ///
    /// The scope is never handed out mutably through the generic boundary: all
    /// mutations flow through the guarded SDK methods below.
    pub fn execution_scope(&self) -> &ExecutionScope {
        &self.vm.host.execution_scope
    }

    /// The current lifecycle phase of this VM's execution scope.
    ///
    /// (The typed generic sibling [`Self::scope_state`] borrows `T`-typed
    /// scope-local state; the lifecycle phase itself is also available through
    /// [`Self::execution_scope`]`.state()`.)
    pub fn scope_phase(&self) -> ScopeState {
        self.vm.host.execution_scope.state()
    }

    // ---- typed scope-state arena --------------------------------------------

    /// Lazily declares `T`-typed scope-local state and returns a mutable
    /// handle to it, creating it with `init` on first access while the scope
    /// is Active.
    ///
    /// Scope state lives in the execution-scope arena and is destroyed by
    /// [`Vm::reset_for_reuse`](super::Vm::reset_for_reuse); persistent module
    /// state (see [`Self::set_module_state`]) survives. A Closing/Quiescent
    /// scope rejects the insert with a structured
    /// [`HostContextErrorKind::Scope`]`(`[`ExecutionScopeError::ScopeClosing`]`)`.
    pub fn scope_state_or_insert_with<T: Send + 'static, F: FnOnce() -> T>(
        &mut self,
        init: F,
    ) -> HostContextResult<&mut T> {
        self.vm
            .host
            .execution_scope
            .scope_state_or_insert_with(init)
            .map_err(HostContextError::from_scope)
    }

    /// Borrows the `T`-typed scope-local state, if present.
    ///
    /// Returns `None` after the terminal close cleared the arena (and for a
    /// type that was never inserted). This is the typed generic sibling of
    /// [`Self::scope_state`], which reports the lifecycle phase.
    pub fn scope_state<T: Send + 'static>(&self) -> Option<&T> {
        self.vm.host.execution_scope.scope_state::<T>()
    }

    /// Mutably borrows the `T`-typed scope-local state, if present.
    pub fn scope_state_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.vm.host.execution_scope.scope_state_mut::<T>()
    }

    /// Removes and returns the `T`-typed scope-local state, if present.
    pub fn take_scope_state<T: Send + 'static>(&mut self) -> Option<T> {
        self.vm.host.execution_scope.take_scope_state::<T>()
    }

    /// Whether the execution scope is still accepting resource / operation
    /// inserts.
    pub fn is_scope_active(&self) -> bool {
        self.vm.host.execution_scope.is_active()
    }

    /// Whether the execution scope reached terminal quiescence.
    pub fn is_scope_quiescent(&self) -> bool {
        self.vm.host.execution_scope.is_quiescent()
    }

    /// Number of live resources in the current execution scope.
    pub fn resource_count(&self) -> usize {
        self.vm.host.execution_scope.resources().len()
    }

    /// Number of occupied operation slots in the current execution scope.
    pub fn operation_count(&self) -> usize {
        self.vm.host.execution_scope.operations().len()
    }

    /// Inserts a typed [`HostResource`] into the current execution scope,
    /// returning its typed capability token.
    ///
    /// A Closing/Quiescent scope rejects the insert with a structured
    /// [`HostContextErrorKind::Scope`]`(`[`ExecutionScopeError::ScopeClosing`]`)`.
    pub fn push_resource<T: HostResource>(&mut self, value: T) -> HostContextResult<Resource<T>> {
        self.vm
            .host
            .execution_scope
            .push_resource(value)
            .map_err(HostContextError::from_scope)
    }

    /// Alias for [`Self::push_resource`], matching the public extension SDK
    /// naming for inserting a typed [`HostResource`] into the current scope.
    pub fn insert_resource<T: HostResource>(&mut self, value: T) -> HostContextResult<Resource<T>> {
        self.push_resource(value)
    }

    /// Starts a host operation in the current execution scope from a full
    /// generic [`OperationSpec`] (concrete [`HostOperation`] driver, optional
    /// deadline, optional cleanup).
    ///
    /// External operations must produce concrete [`HostOperation`] drivers:
    /// the scope and its registry own poll/cancel, so a driver is the only
    /// thing the extension supplies. There is deliberately no second registry
    /// and no adapter-specific generic helper on this surface.
    pub fn start_operation(&mut self, spec: OperationSpec) -> HostContextResult<OperationId> {
        self.vm
            .host
            .execution_scope
            .start_operation(spec)
            .map_err(HostContextError::from_scope)
    }

    /// Cancels one started operation by id, forwarding the reason to its
    /// concrete driver. Returns `false` when the operation was already
    /// terminal.
    pub fn cancel_operation(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> HostContextResult<bool> {
        self.vm
            .host
            .execution_scope
            .cancel_operation(id, reason)
            .map_err(HostContextError::from_scope)
    }

    /// Marks an operation completed without polling. The terminal slot remains
    /// occupied until [`take_operation_outcome`](Self::take_operation_outcome).
    pub fn complete_operation(&mut self, id: OperationId) -> HostContextResult<bool> {
        self.vm
            .host
            .execution_scope
            .complete_operation(id)
            .map_err(HostContextError::from_scope)
    }

    /// Consumes one terminal outcome and releases its slot for generation
    /// reuse.
    pub fn take_operation_outcome(
        &mut self,
        id: OperationId,
    ) -> HostContextResult<OperationOutcome> {
        self.vm
            .host
            .execution_scope
            .take_operation_outcome(id)
            .map_err(HostContextError::from_scope)
    }

    /// Drives one operation to terminal, polling its concrete driver.
    pub fn poll_operation(
        &mut self,
        id: OperationId,
        cx: &mut Context<'_>,
    ) -> Poll<HostContextResult<OperationOutcome>> {
        match self.vm.host.execution_scope.poll_operation(id, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.map_err(HostContextError::from_scope)),
        }
    }

    /// Aborts a started operation after a later handoff step fails. The driver
    /// is cancelled at most once, the occupied registry slot is released, and
    /// the id becomes stale as one atomic scope lifecycle action.
    pub fn abort_operation(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> HostContextResult<bool> {
        self.vm
            .host
            .execution_scope
            .abort_operation(id, reason)
            .map_err(HostContextError::from_scope)
    }

    /// Reads the status of one operation.
    pub fn operation_status(&self, id: OperationId) -> HostContextResult<OperationStatus> {
        self.vm
            .host
            .execution_scope
            .operations()
            .status(id)
            .map_err(HostContextError::from_operation)
    }

    /// Closes one resource in the current execution scope via the generic
    /// table contract. A `Pending` close is driven to completion by the usual
    /// scope poll machinery.
    pub fn close_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        reason: ResourceCloseReason,
    ) -> HostContextResult<CloseProgress> {
        self.vm
            .host
            .execution_scope
            .close_resource::<T>(handle, reason)
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
        self.vm
            .host
            .execution_scope
            .resources()
            .get(token)
            .map_err(HostContextError::from_resource)
    }

    /// Validates the concrete resource declaration against a catalog key
    /// before touching a resource handle.
    pub fn validate_resource_type_key<T: HostResource>(
        &self,
        expected: &ResourceTypeKey,
    ) -> HostContextResult<()> {
        ResourceTable::validate_concrete_resource_type_key::<T>(expected)
            .map_err(HostContextError::from_resource)
    }

    /// Recovers a typed token only after validating both the concrete
    /// declaration key and the live handle's key.
    pub fn typed_resource_with_key<T: HostResource>(
        &self,
        handle: ResourceHandle,
        expected: &ResourceTypeKey,
    ) -> HostContextResult<Resource<T>> {
        self.validate_resource_type_key::<T>(expected)?;
        self.vm
            .host
            .execution_scope
            .resources()
            .validate_resource_type_key(handle, expected)
            .map_err(HostContextError::from_resource)?;
        self.typed_resource::<T>(handle)
    }

    /// Borrows a resource after exact catalog-key validation.
    pub fn borrow_resource_with_key<T: HostResource>(
        &self,
        handle: ResourceHandle,
        expected: &ResourceTypeKey,
    ) -> HostContextResult<ResourceRef<'_, T>> {
        let token = self.typed_resource_with_key::<T>(handle, expected)?;
        self.resource(&token)
    }

    /// Mutably borrows a resource after exact catalog-key validation.
    pub fn borrow_resource_mut_with_key<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        expected: &ResourceTypeKey,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        self.validate_resource_type_key::<T>(expected)?;
        self.vm
            .host
            .execution_scope
            .resources()
            .validate_resource_type_key(handle, expected)
            .map_err(HostContextError::from_resource)?;
        self.borrow_resource_mut::<T>(handle)
    }

    /// Takes a resource after exact catalog-key validation. A mismatch is
    /// returned before the table removes or mutates the resource.
    pub fn take_resource_with_key<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        expected: &ResourceTypeKey,
    ) -> HostContextResult<T> {
        self.validate_resource_type_key::<T>(expected)?;
        self.vm
            .host
            .execution_scope
            .resources()
            .validate_resource_type_key(handle, expected)
            .map_err(HostContextError::from_resource)?;
        self.take_resource::<T>(handle)
    }

    /// Alias for [`Self::take_resource_with_key`].
    pub fn take_owned_with_key<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        expected: &ResourceTypeKey,
    ) -> HostContextResult<T> {
        self.take_resource_with_key::<T>(handle, expected)
    }

    /// Mutably borrows a typed resource for the duration of this synchronous
    /// host call.
    pub fn resource_mut<T: HostResource>(
        &mut self,
        token: &Resource<T>,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        self.vm
            .host
            .execution_scope
            .resources_mut()
            .get_mut(token)
            .map_err(HostContextError::from_resource)
    }

    /// Validates a raw [`ResourceHandle`] against the current scope and
    /// recovers a typed token (read-only).
    pub fn typed_resource<T: HostResource>(
        &self,
        handle: ResourceHandle,
    ) -> HostContextResult<Resource<T>> {
        self.vm
            .host
            .execution_scope
            .resources()
            .typed(handle)
            .map_err(HostContextError::from_resource)
    }

    /// Borrow a raw handle after typed arena/generation validation.
    pub fn borrow_resource<T: HostResource>(
        &self,
        handle: ResourceHandle,
    ) -> HostContextResult<ResourceRef<'_, T>> {
        let token = self.typed_resource::<T>(handle)?;
        self.resource(&token)
    }

    /// Mutably borrow a raw handle after typed arena/generation validation.
    pub fn borrow_resource_mut<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
    ) -> HostContextResult<ResourceMut<'_, T>> {
        let token = self.typed_resource::<T>(handle)?;
        self.resource_mut(&token)
    }

    /// Takes a raw typed resource out of the current execution scope and
    /// transfers the concrete value to the caller. The handle is validated for
    /// arena, generation, state, and concrete type before removal.
    pub fn take_resource<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
    ) -> HostContextResult<T> {
        self.vm
            .host
            .execution_scope
            .take_resource::<T>(handle)
            .map_err(HostContextError::from_scope)
    }

    /// Alias for [`Self::take_resource`] matching the TakeOwned terminology in
    /// host-function schemas.
    pub fn take_owned<T: HostResource>(&mut self, handle: ResourceHandle) -> HostContextResult<T> {
        self.take_resource::<T>(handle)
    }

    /// Begins closing the resource through the generic table contract.
    ///
    /// This is the generic "close one resource" adapter (host-agnostic): the
    /// resource arena/type/generation/live checks and `begin_close` happen
    /// before any state mutation, so a rejected close leaves the table
    /// untouched.
    pub fn begin_close<T: HostResource>(
        &mut self,
        handle: ResourceHandle,
        reason: ResourceCloseReason,
    ) -> HostContextResult<CloseProgress> {
        self.close_resource::<T>(handle, reason)
    }
}

impl HostRuntime {
    /// Registers typed per-VM module state, replacing any earlier value of the
    /// same type.
    pub(crate) fn set_module_state<M: HostModule>(&mut self, state: M) -> bool {
        self.module_state_store.set(state)
    }

    /// Borrows the registered typed module state, if any.
    pub(crate) fn get_module_state<M: HostModule>(&self) -> Option<&M> {
        self.module_state_store.get()
    }

    /// Borrows the registered typed module state mutably, if any.
    pub(crate) fn get_module_state_mut<M: HostModule>(&mut self) -> Option<&mut M> {
        self.module_state_store.get_mut()
    }

    /// Removes and returns the registered typed module state, if any.
    pub(crate) fn remove_module_state<M: HostModule>(&mut self) -> Option<M> {
        self.module_state_store.remove()
    }

    /// Returns `true` when no module state is currently registered.
    pub(crate) fn is_module_state_empty(&self) -> bool {
        self.module_state_store.is_empty()
    }
}
