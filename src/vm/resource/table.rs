//! Host-agnostic typed generational resource table.
//!
//! The table is the single owner of every erased [`HostResource`] for one
//! execution scope. It manages:
//!
//! - a bounded [`ResourceHandle`] space (arena + slot + generation),
//! - [`std::any::TypeId`] based borrow-time type validation,
//! - parent/child links for typed relational resources,
//! - poll-based two-phase close with deterministic child-first shutdown.
//!
//! The table holds no concrete resource type: host crates register resources
//! through [`HostResource`] and the core never dispatches on a class. The table
//! is `Send + !Sync`: it is moved under the sole mutating VM/scope owner.

use std::any::{Any, TypeId};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use super::close::{CloseProgress, HostResource};
use super::error::{ResourceError, ResourceErrorCode, ResourceResult};
use super::handle::{
    DEFAULT_MAX_RESOURCES, MAX_HANDLE_ARENA_ID, MAX_HANDLE_GENERATION, MAX_RESOURCE_SLOTS,
    Resource, ResourceHandle, ResourceMut, ResourceRef,
};
use super::reason::ResourceCloseReason;

/// Process-unique arena identity source, never recycled.
///
/// An arena id therefore binds a handle to one table (and the scope that owns
/// it) for the lifetime of the process.
static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(1);

/// Lifecycle of one slot.
enum SlotState {
    Vacant,
    Open(Box<dyn HostResource>),
    /// `begin_close` returned [`CloseProgress::Pending`]; the resource is being
    /// polled to completion and its generation is not yet reusable.
    Closing(Box<dyn HostResource>),
}

struct ResourceSlot {
    /// Advanced on every reuse.
    generation: u32,
    /// Concrete type of the current occupant; borrow-time validation only.
    type_id: TypeId,
    /// Handle of the parent, if this resource is a child.
    parent: Option<ResourceHandle>,
    /// Live child handles. A child is removed only once its close is fully
    /// finished and the slot is vacant again.
    children: BTreeSet<ResourceHandle>,
    state: SlotState,
}

/// Cumulative state persisted across [`ResourceTable::poll_close_all`] polls
/// until the table is quiescent.
struct CloseAllState {
    reason: ResourceCloseReason,
    closed: usize,
    first_error: Option<ResourceError>,
}

/// Bounded arena of erased resources owned by one execution scope.
///
/// `Send + !Sync` by construction: it must never be shared; the owning scope
/// moves it and mutates it single-threaded.
pub struct ResourceTable {
    arena_id: u64,
    max_entries: usize,
    slots: Vec<ResourceSlot>,
    vacant_slots: Vec<usize>,
    active_entries: usize,
    /// In-flight `poll_close_all` sweep, if one is active.
    close_all: Option<CloseAllState>,
}

impl ResourceTable {
    /// Creates an empty table with a fresh arena identity and capacity limit.
    pub fn with_limit(max_entries: usize) -> ResourceResult<Self> {
        if max_entries == 0 || max_entries > MAX_RESOURCE_SLOTS {
            return Err(ResourceError::new(
                ResourceErrorCode::InvalidConfiguration,
                "resource::table",
                format!("resource table capacity must be between 1 and {MAX_RESOURCE_SLOTS}"),
            )
            .with_limit(MAX_RESOURCE_SLOTS));
        }
        let arena_id = NEXT_ARENA_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |arena_id| {
                (arena_id <= MAX_HANDLE_ARENA_ID).then_some(arena_id + 1)
            })
            .map_err(|_| {
                ResourceError::new(
                    ResourceErrorCode::ResourceIdExhausted,
                    "resource::table",
                    "resource table arena identity space is exhausted",
                )
            })?;
        Ok(Self {
            arena_id,
            max_entries,
            slots: Vec::new(),
            vacant_slots: Vec::new(),
            active_entries: 0,
            close_all: None,
        })
    }

    /// Creates a table with the default [`DEFAULT_MAX_RESOURCES`] capacity.
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_MAX_RESOURCES).expect("default table configuration is valid")
    }

    /// Number of currently live (open or closing) resources.
    pub fn len(&self) -> usize {
        self.active_entries
    }

    /// Whether the table currently holds no live resources.
    pub fn is_empty(&self) -> bool {
        self.active_entries == 0
    }

    /// Inserts a root resource and returns its typed token.
    pub fn push<T: HostResource>(&mut self, value: T) -> ResourceResult<Resource<T>> {
        let handle = self.allocate(None, value)?;
        Ok(Resource::from_handle(handle))
    }

    /// Inserts a child resource linked to `parent`.
    ///
    /// The parent must be an open resource of type `P`. The child cannot be
    /// registered while its parent is closing, and the parent cannot be closed
    /// while the child is live.
    pub fn push_child<T: HostResource, P: HostResource>(
        &mut self,
        value: T,
        parent: &Resource<P>,
    ) -> Result<Resource<T>, ResourceError> {
        let parent_handle = parent.handle();
        // Validate the parent before allocating, so a bad parent key leaves no
        // orphan behind.
        self.validate_open::<P>(parent_handle)?;
        let child_handle = self.allocate(Some(parent_handle), value)?;
        let parent_index = self.resolve_index(parent_handle)?;
        self.slots[parent_index].children.insert(child_handle);
        Ok(Resource::from_handle(child_handle))
    }

    /// Validates a raw [`ResourceHandle`] and recovers a typed token.
    ///
    /// This is the only public way to lift an arbitrary raw handle (for example
    /// one stored inside a script value) into a typed [`Resource<T>`]. It
    /// rejects the handle if it belongs to a different table (arena), refers to
    /// a stale slot generation, names the wrong concrete `TypeId`, or points at
    /// a resource that is no longer `Open`:
    ///
    /// - foreign arena → [`ResourceErrorCode::ResourceHandleWrongTable`]
    /// - stale generation → [`ResourceErrorCode::ResourceStale`]
    /// - wrong type → [`ResourceErrorCode::ResourceTypeMismatch`]
    /// - closed/closing → [`ResourceErrorCode::ResourceAlreadyClosed`]
    ///
    /// A rejected recovery is purely read-only: no slot, generation, link, or
    /// type state is mutated.
    pub fn typed<T: HostResource>(&self, handle: ResourceHandle) -> ResourceResult<Resource<T>> {
        // Parentheses drop the index: validation is the sole purpose here.
        let _slot_index = self.validate_active::<T>(handle)?;
        Ok(Resource::from_handle(handle))
    }

    /// Immutably borrows one live resource for the duration of a host call.
    pub fn get<T: HostResource>(
        &self,
        resource: &Resource<T>,
    ) -> ResourceResult<ResourceRef<'_, T>> {
        let handle = resource.handle();
        let slot_index = self.validate_active::<T>(handle)?;
        let slot = &self.slots[slot_index];
        let SlotState::Open(resource) = &slot.state else {
            return Err(already_closed_error(handle));
        };
        let value = (resource.as_ref() as &dyn Any)
            .downcast_ref::<T>()
            .ok_or_else(|| type_mismatch(handle, TypeId::of::<T>()))?;
        Ok(ResourceRef::new(handle, value))
    }

    /// Mutably borrows one live resource for the duration of a host call.
    pub fn get_mut<T: HostResource>(
        &mut self,
        resource: &Resource<T>,
    ) -> ResourceResult<ResourceMut<'_, T>> {
        let handle = resource.handle();
        let slot_index = self.validate_active::<T>(handle)?;
        let slot = &mut self.slots[slot_index];
        let SlotState::Open(resource) = &mut slot.state else {
            return Err(already_closed_error(handle));
        };
        let value = (resource.as_mut() as &mut dyn Any)
            .downcast_mut::<T>()
            .ok_or_else(|| type_mismatch(handle, TypeId::of::<T>()))?;
        Ok(ResourceMut::new(handle, value))
    }

    /// Begins closing a resource.
    ///
    /// Properties:
    /// - A parent with any live child returns
    ///   [`ResourceErrorCode::ResourceHasChildren`].
    /// - An already-closing resource returns [`CloseProgress::Pending`]
    ///   (idempotent); the generation is held until close finishes.
    /// - `CloseProgress::Ready` means the slot is already vacant again and the
    ///   generation advanced.
    pub fn begin_close<T: HostResource>(
        &mut self,
        resource: Resource<T>,
        reason: ResourceCloseReason,
    ) -> ResourceResult<CloseProgress> {
        let handle = resource.handle();
        let slot_index = self.resolve_index(handle)?;
        self.check_generation(slot_index, handle)?;
        self.check_type::<T>(slot_index, handle)?;

        let state = std::mem::replace(&mut self.slots[slot_index].state, SlotState::Vacant);
        match state {
            SlotState::Open(mut resource) => {
                if !self.slots[slot_index].children.is_empty() {
                    self.slots[slot_index].state = SlotState::Open(resource);
                    return Err(has_children_error(handle));
                }
                match resource.begin_close(reason) {
                    Ok(CloseProgress::Ready) => {
                        self.reclaim(slot_index);
                        Ok(CloseProgress::Ready)
                    }
                    Ok(CloseProgress::Pending) => {
                        self.slots[slot_index].state = SlotState::Closing(resource);
                        Ok(CloseProgress::Pending)
                    }
                    Err(error) => {
                        self.reclaim(slot_index);
                        Err(error)
                    }
                }
            }
            SlotState::Closing(resource) => {
                // Idempotent: the close is already in flight; keep holding the
                // generation until the outer caller drives poll_close.
                self.slots[slot_index].state = SlotState::Closing(resource);
                Ok(CloseProgress::Pending)
            }
            SlotState::Vacant => Err(already_closed_error(handle)),
        }
    }

    /// Polls one in-progress close to completion.
    ///
    /// Returns `Ready(Ok(()))` on a clean finish, `Ready(Err(_))` on a cleanup
    /// failure (the slot is still reclaimed), or `Pending` while the resource
    /// needs more time.
    pub fn poll_close<T: HostResource>(
        &mut self,
        resource: Resource<T>,
        cx: &mut Context<'_>,
    ) -> Poll<ResourceResult<()>> {
        let handle = resource.handle();
        let slot_index = self.resolve_index(handle)?;
        self.check_generation(slot_index, handle)?;
        self.check_type::<T>(slot_index, handle)?;

        let state = std::mem::replace(&mut self.slots[slot_index].state, SlotState::Vacant);
        match state {
            SlotState::Closing(mut resource) => match resource.poll_close(cx) {
                Poll::Ready(result) => {
                    self.reclaim(slot_index);
                    Poll::Ready(result)
                }
                Poll::Pending => {
                    self.slots[slot_index].state = SlotState::Closing(resource);
                    Poll::Pending
                }
            },
            SlotState::Open(resource) => {
                // Not closing: restore the open resource and report the precise
                // wrong-state error (distinct from an invalid handle).
                self.slots[slot_index].state = SlotState::Open(resource);
                Poll::Ready(Err(not_closing_error(handle)))
            }
            SlotState::Vacant => Poll::Ready(Err(already_closed_error(handle))),
        }
    }

    /// Drives a caller-context close of every live resource, child first.
    ///
    /// This is the event-driven close-all: unlike a synchronous sweep it can
    /// wait on genuinely `Pending` resources using the caller's waker. Leaves
    /// close before their parents (post-order). A cleanup failure does not stop
    /// the remaining best-effort closes: every resource close is attempted and
    /// the first failure is retained until the whole sweep finishes.
    ///
    /// Contract:
    /// - Returns [`Poll::Ready`] **only** once the table is quiescent
    ///   ([`len`](ResourceTable::len) `== 0`). `Ready(Ok(n))` reports the
    ///   cumulative number of resources closed across all polls; `Ready(Err)`
    ///   reports the first cleanup failure once every resource has finished.
    /// - Returns [`Poll::Pending`] whenever any Open or Closing resource
    ///   remains. The cumulative closed count, the first cleanup error, and the
    ///   initial `reason` are persisted across Pending polls.
    /// - The `reason` is bound on the first poll of a sweep. Supplying a
    ///   conflicting reason is rejected deterministically with
    ///   [`ResourceErrorCode::ResourceCloseInProgress`] and leaves the in-flight
    ///   sweep (and its original reason) untouched.
    ///
    /// ```ignore
    /// let mut cx = Context::from_waker(&waker);
    /// loop {
    ///     match table.poll_close_all(reason, &mut cx) {
    ///         Poll::Ready(result) => break result,
    ///         Poll::Pending => /* yield; woken when a resource makes progress */,
    ///     }
    /// }
    /// ```
    pub fn poll_close_all(
        &mut self,
        reason: ResourceCloseReason,
        cx: &mut Context<'_>,
    ) -> Poll<ResourceResult<usize>> {
        // Deterministically reject a conflicting reason. The in-flight sweep
        // keeps the reason it started with; we do not mutate any state here.
        if let Some(state) = self.close_all.as_ref()
            && state.reason != reason
        {
            return Poll::Ready(Err(close_in_progress_error(reason, state.reason)));
        }
        if self.close_all.is_none() {
            self.close_all = Some(CloseAllState {
                reason,
                closed: 0,
                first_error: None,
            });
        }
        let reason = self.close_all.as_ref().unwrap().reason;
        let mut closed = self.close_all.as_ref().unwrap().closed;
        let mut first_error = self.close_all.as_ref().unwrap().first_error.clone();

        // Sweep until a full pass makes no progress: every current leaf is
        // begun, every Closing resource is polled, and both repeat until the
        // state stabilizes. Genuinely-Pending resources stay in `Closing` and
        // are re-polled on a later `poll_close_all` call with the real waker.
        let mut progressed = true;
        while progressed {
            progressed = false;
            let mut leaf_indices = self.open_leaf_indices();
            leaf_indices.sort_unstable();
            for slot_index in leaf_indices {
                if self
                    .slots
                    .get(slot_index)
                    .is_none_or(|slot| !matches!(slot.state, SlotState::Open(_)))
                {
                    continue;
                }
                progressed |=
                    self.try_begin_close(slot_index, reason, &mut closed, &mut first_error);
            }
            let closing_indices = self.closing_indices();
            for slot_index in closing_indices {
                progressed |= self.try_poll_close(slot_index, cx, &mut closed, &mut first_error);
            }
        }

        // Persist cumulative progress across Pending polls.
        let state = self.close_all.as_mut().unwrap();
        state.closed = closed;
        state.first_error = first_error;

        if self.is_empty() {
            // Quiescent: this, and only this, warrants a Ready completion.
            let state = self.close_all.take().unwrap();
            match state.first_error {
                Some(error) => Poll::Ready(Err(error)),
                None => Poll::Ready(Ok(state.closed)),
            }
        } else {
            Poll::Pending
        }
    }

    /// Best-effort synchronous child-first close of every live resource.
    ///
    /// Drives a single [`poll_close_all`](ResourceTable::poll_close_all) sweep
    /// with a no-op waker and returns only once the table is quiescent:
    /// - `Ready(Ok(n))` is reported exactly when [`len`](ResourceTable::len)
    ///   reached zero and every close succeeded;
    /// - `Ready(Err(_))` is reported when every resource finished but the first
    ///   cleanup failed;
    /// - [`ResourceErrorCode::ResourceClosePending`] is returned (never success)
    ///   when at least one resource remains pending at the end of the single
    ///   no-op sweep, because such a resource needs an external waker that a
    ///   synchronous no-op driver cannot provide.
    ///
    /// For genuinely event-driven resources use
    /// [`poll_close_all`](ResourceTable::poll_close_all) so their waker is
    /// honored.
    pub fn close_all(&mut self, reason: ResourceCloseReason) -> ResourceResult<usize> {
        let mut cx = noop_context();
        match self.poll_close_all(reason, &mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(ResourceError::new(
                ResourceErrorCode::ResourceClosePending,
                "resource::close_all",
                "synchronous close-all cannot drive pending resources to quiescence",
            )),
        }
    }

    /// Returns the process-unique arena identity of this table.
    pub fn arena_id(&self) -> u64 {
        self.arena_id
    }

    // ---- internal close machinery -------------------------------------------------

    fn try_begin_close(
        &mut self,
        slot_index: usize,
        reason: ResourceCloseReason,
        closed: &mut usize,
        first_error: &mut Option<ResourceError>,
    ) -> bool {
        let state = std::mem::replace(&mut self.slots[slot_index].state, SlotState::Vacant);
        let SlotState::Open(mut resource) = state else {
            // Not open (e.g. already closing); restore and report no progress.
            self.slots[slot_index].state = state;
            return false;
        };
        if !self.slots[slot_index].children.is_empty() {
            self.slots[slot_index].state = SlotState::Open(resource);
            return false;
        }
        match resource.begin_close(reason) {
            Ok(CloseProgress::Ready) => {
                self.reclaim(slot_index);
                *closed += 1;
                true
            }
            Ok(CloseProgress::Pending) => {
                self.slots[slot_index].state = SlotState::Closing(resource);
                true
            }
            Err(error) => {
                self.reclaim(slot_index);
                *closed += 1;
                first_error.get_or_insert(error);
                true
            }
        }
    }

    fn try_poll_close(
        &mut self,
        slot_index: usize,
        cx: &mut Context<'_>,
        closed: &mut usize,
        first_error: &mut Option<ResourceError>,
    ) -> bool {
        let state = std::mem::replace(&mut self.slots[slot_index].state, SlotState::Vacant);
        let SlotState::Closing(mut resource) = state else {
            self.slots[slot_index].state = state;
            return false;
        };
        match resource.poll_close(cx) {
            Poll::Ready(result) => {
                self.reclaim(slot_index);
                *closed += 1;
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
                true
            }
            Poll::Pending => {
                self.slots[slot_index].state = SlotState::Closing(resource);
                false
            }
        }
    }

    fn reclaim(&mut self, slot_index: usize) {
        let generation = self.slots[slot_index].generation;
        let parent = self.slots[slot_index].parent.take();
        if let Some(parent_handle) = parent
            && let Some(child_handle) =
                ResourceHandle::encode(self.arena_id, slot_index, u64::from(generation))
            && let Ok(parent_index) = self.resolve_index(parent_handle)
        {
            self.slots[parent_index].children.remove(&child_handle);
        }
        self.slots[slot_index].state = SlotState::Vacant;
        if u64::from(self.slots[slot_index].generation) < MAX_HANDLE_GENERATION {
            self.vacant_slots.push(slot_index);
        }
        self.active_entries -= 1;
    }

    /// Indices of slots currently in [`SlotState::Open`] with no live children.
    fn open_leaf_indices(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                (matches!(slot.state, SlotState::Open(_)) && slot.children.is_empty())
                    .then_some(index)
            })
            .collect()
    }

    /// Indices of slots currently in [`SlotState::Closing`].
    fn closing_indices(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                matches!(slot.state, SlotState::Closing(_)).then_some(index)
            })
            .collect()
    }

    // ---- allocation ---------------------------------------------------------------

    fn allocate<T: HostResource>(
        &mut self,
        parent: Option<ResourceHandle>,
        value: T,
    ) -> Result<ResourceHandle, ResourceError> {
        if self.active_entries >= self.max_entries {
            return Err(ResourceError::new(
                ResourceErrorCode::ResourceLimitExceeded,
                "resource::push",
                "resource table capacity has been reached",
            )
            .with_limit(self.max_entries));
        }

        let type_id = TypeId::of::<T>();
        let value: Box<dyn HostResource> = Box::new(value);

        let (slot_index, generation) = if let Some(slot_index) = self.vacant_slots.pop() {
            let generation = self.slots[slot_index]
                .generation
                .checked_add(1)
                .filter(|generation| u64::from(*generation) <= MAX_HANDLE_GENERATION)
                .expect("only reusable generations enter the vacant list");
            self.slots[slot_index].generation = generation;
            self.slots[slot_index].type_id = type_id;
            self.slots[slot_index].parent = parent;
            self.slots[slot_index].children.clear();
            self.slots[slot_index].state = SlotState::Open(value);
            (slot_index, generation)
        } else {
            if self.slots.len() >= MAX_RESOURCE_SLOTS {
                return Err(ResourceError::new(
                    ResourceErrorCode::ResourceIdExhausted,
                    "resource::push",
                    "resource table slot space is exhausted",
                ));
            }
            let slot_index = self.slots.len();
            let generation = 1u32;
            self.slots.push(ResourceSlot {
                generation,
                type_id,
                parent,
                children: BTreeSet::new(),
                state: SlotState::Open(value),
            });
            (slot_index, generation)
        };
        self.active_entries += 1;
        ResourceHandle::encode(self.arena_id, slot_index, u64::from(generation)).ok_or_else(|| {
            ResourceError::new(
                ResourceErrorCode::ResourceIdExhausted,
                "resource::push",
                "resource handle encoding overflowed",
            )
        })
    }

    fn resolve_index(&self, handle: ResourceHandle) -> ResourceResult<usize> {
        if handle.arena_id() != self.arena_id {
            return Err(wrong_arena_error(handle));
        }
        let slot_index = handle.slot_index()?;
        if slot_index >= self.slots.len() {
            return Err(stale_handle_error(handle));
        }
        self.check_generation(slot_index, handle)?;
        Ok(slot_index)
    }

    fn check_generation(&self, slot_index: usize, handle: ResourceHandle) -> ResourceResult<()> {
        if u64::from(self.slots[slot_index].generation) != handle.generation() {
            return Err(stale_handle_error(handle));
        }
        Ok(())
    }

    fn check_type<T: 'static>(
        &self,
        slot_index: usize,
        handle: ResourceHandle,
    ) -> ResourceResult<()> {
        if self.slots[slot_index].type_id != TypeId::of::<T>() {
            return Err(type_mismatch(handle, TypeId::of::<T>()));
        }
        Ok(())
    }

    /// Validates that the handle points at a live, open resource of the given
    /// concrete type.
    fn validate_active<T: 'static>(&self, handle: ResourceHandle) -> ResourceResult<usize> {
        let slot_index = self.resolve_index(handle)?;
        self.check_type::<T>(slot_index, handle)?;
        if !matches!(self.slots[slot_index].state, SlotState::Open(_)) {
            return Err(already_closed_error(handle));
        }
        Ok(slot_index)
    }

    fn validate_open<T: 'static>(&self, handle: ResourceHandle) -> ResourceResult<()> {
        let slot_index = self.resolve_index(handle)?;
        self.check_type::<T>(slot_index, handle)?;
        match self.slots[slot_index].state {
            SlotState::Open(_) => Ok(()),
            SlotState::Closing(_) | SlotState::Vacant => Err(already_closed_error(handle)),
        }
    }
}

impl Default for ResourceTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ResourceTable {
    fn drop(&mut self) {
        // Best-effort last-resort cleanup with a no-op waker. This performs at
        // most one synchronous sweep; it explicitly does NOT claim quiescence.
        // In the intended flow the owning scope drives poll-based close to
        // quiescence via `poll_close_all` before dropping the table, so this
        // path only catches resources whose close was never driven. Genuinely
        // event-driven Pending resources may remain live here and are released
        // by their own `Drop` guards.
        let _ = self.close_all(ResourceCloseReason::VmReset);
    }
}

// ---- error constructors ------------------------------------------------------------

fn wrong_arena_error(handle: ResourceHandle) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceHandleWrongTable,
        "resource::table",
        "resource handle does not belong to this table's arena",
    )
    .with_value(handle.raw())
}

fn stale_handle_error(handle: ResourceHandle) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceStale,
        "resource::table",
        "resource handle refers to a stale slot generation",
    )
    .with_value(handle.raw())
}

fn already_closed_error(handle: ResourceHandle) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceAlreadyClosed,
        "resource::table",
        "resource is already closed or closing",
    )
    .with_value(handle.raw())
}

fn type_mismatch(handle: ResourceHandle, expected: TypeId) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceTypeMismatch,
        "resource::table",
        format!("resource type does not match expected type {:?}", expected),
    )
    .with_value(handle.raw())
}

fn has_children_error(handle: ResourceHandle) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceHasChildren,
        "resource::table",
        "resource cannot close while it has live child resources",
    )
    .with_value(handle.raw())
}

fn not_closing_error(handle: ResourceHandle) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceNotClosing,
        "resource::table",
        "resource is not in the closing state",
    )
    .with_value(handle.raw())
}

fn close_in_progress_error(
    reason: ResourceCloseReason,
    in_progress: ResourceCloseReason,
) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceCloseInProgress,
        "resource::poll_close_all",
        format!(
            "a close-all sweep is already in progress with reason `{in_progress}`; \
             requested reason `{reason}` was rejected"
        ),
    )
}

// ---- noop waker for synchronous poll driving ---------------------------------------

/// A `'static` context with a no-op waker, used to drive poll-based close to
/// completion inside the synchronous `close_all` sweep. Resources closed in
/// this path are expected to complete without external wakeup.
fn noop_context() -> Context<'static> {
    Context::from_waker(core::task::Waker::noop())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

    const REASON: ResourceCloseReason = ResourceCloseReason::ResourceClosed;

    /// A resource that counts synchronous closes.
    struct UnitRes(Arc<AtomicUsize>);

    impl UnitRes {
        fn new() -> (Self, Arc<AtomicUsize>) {
            let closes = Arc::new(AtomicUsize::new(0));
            (Self(closes.clone()), closes)
        }
    }

    impl HostResource for UnitRes {
        fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CloseProgress::Ready)
        }
    }

    /// A distinct inert type used to mint a mismatched `Resource<Other>`.
    struct OtherRes;

    impl HostResource for OtherRes {}

    fn poll_err(poll: Poll<ResourceResult<()>>) -> ResourceErrorCode {
        match poll {
            Poll::Ready(Err(error)) => error.code(),
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    #[test]
    fn typed_recovery_with_crate_private_resource_constructor_is_consistent() {
        let mut table = ResourceTable::new();
        let (res, closes) = UnitRes::new();
        let token = table.push(res).unwrap();

        // Public validated recovery returns an equivalent token.
        let recovered = table.typed::<UnitRes>(token.handle()).expect("recovery");
        assert_eq!(recovered.handle(), token.handle());
        table.get(&recovered).expect("recovered token borrows");

        // The crate-private constructor is only reachable inside this crate,
        // and `typed` is the checked path; constructing a mismatched token here
        // is exactly what unit tests may do to exercise rejection logic.
        let wrong: Resource<OtherRes> = Resource::from_handle(token.handle());
        assert_eq!(
            table.get(&wrong).unwrap_err().code(),
            ResourceErrorCode::ResourceTypeMismatch
        );
        assert_eq!(
            table.get_mut(&wrong).unwrap_err().code(),
            ResourceErrorCode::ResourceTypeMismatch
        );
        assert_eq!(table.len(), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        table.get(&token).expect("real token unaffected");
    }

    #[test]
    fn begin_close_rejects_mismatched_type_without_firing_close() {
        let mut table = ResourceTable::new();
        let (res, closes) = UnitRes::new();
        let token = table.push(res).unwrap();
        let wrong: Resource<OtherRes> = Resource::from_handle(token.handle());

        assert_eq!(
            table.begin_close(wrong, REASON).unwrap_err().code(),
            ResourceErrorCode::ResourceTypeMismatch
        );
        assert_eq!(table.len(), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 0);

        // The real token still closes exactly once.
        assert_eq!(
            table.begin_close(token, REASON).unwrap(),
            CloseProgress::Ready
        );
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn poll_close_distinguishes_not_closing_vacant_and_mismatched_type() {
        let mut table = ResourceTable::new();
        let (res, _) = UnitRes::new();
        let token = table.push(res).unwrap();
        let handle = token.handle();
        let mut cx = noop_context();

        // Open resource must report ResourceNotClosing, not InvalidResourceHandle.
        assert_eq!(
            poll_err(table.poll_close(token, &mut cx)),
            ResourceErrorCode::ResourceNotClosing
        );
        // And it stays open, unmutated, and fully usable.
        assert_eq!(table.len(), 1);
        table.get(&token).expect("still open");

        // Mismatched type on poll_close -> type mismatch.
        let wrong: Resource<OtherRes> = Resource::from_handle(handle);
        assert_eq!(
            poll_err(table.poll_close(wrong, &mut cx)),
            ResourceErrorCode::ResourceTypeMismatch
        );

        // After a synchronous-close the slot is vacant at the same generation,
        // so poll_close reports ResourceAlreadyClosed (precise, not generic).
        assert_eq!(
            table.begin_close(token, REASON).unwrap(),
            CloseProgress::Ready
        );
        assert_eq!(
            poll_err(table.poll_close(token, &mut cx)),
            ResourceErrorCode::ResourceAlreadyClosed
        );
    }

    #[test]
    fn push_child_rejects_wrong_parent_type_and_closed_parent() {
        let mut table = ResourceTable::new();
        let parent = table.push(UnitRes::new().0).unwrap();
        let wrong_parent: Resource<OtherRes> = Resource::from_handle(parent.handle());

        assert_eq!(
            table
                .push_child(UnitRes::new().0, &wrong_parent)
                .unwrap_err()
                .code(),
            ResourceErrorCode::ResourceTypeMismatch
        );
        // No orphan child was left behind.
        assert_eq!(table.len(), 1);

        // A closed parent (vacant slot, same generation) rejects new children.
        let parent_handle = parent.handle();
        assert_eq!(
            table.begin_close(parent, REASON).unwrap(),
            CloseProgress::Ready
        );
        let stale_parent: Resource<UnitRes> = Resource::from_handle(parent_handle);
        assert_eq!(
            table
                .push_child(UnitRes::new().0, &stale_parent)
                .unwrap_err()
                .code(),
            ResourceErrorCode::ResourceAlreadyClosed
        );
        assert_eq!(table.len(), 0);
    }
}
