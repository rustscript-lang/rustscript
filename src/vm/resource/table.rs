//! Host-agnostic typed generational resource table.
//!
//! The table is the single owner of every erased [`HostResource`] for one
//! execution scope. It manages:
//!
//! - a bounded [`ResourceHandle`] space (arena + slot + generation),
//! - [`std::any::TypeId`] based borrow-time type validation,
//! - poll-based two-phase close with deterministic shutdown.
//!
//! The table holds no concrete resource type: host crates register resources
//! through [`HostResource`] and the core never dispatches on a class. The table
//! is `Send + !Sync`: it is moved under the sole mutating VM/scope owner.

use std::any::{Any, TypeId};
use std::cell::{Cell, Ref, RefCell, RefMut};
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

/// Test-only, per-thread arena-id source override.
///
/// Exhaustion is a *process-global* property: the real `NEXT_ARENA_ID` counter
/// can only reach `MAX_HANDLE_ARENA_ID` after ~1,048,575 tables have been
/// created in one process, which no test suite can (or should) reproduce
/// deterministically. Exhaustion tests therefore install a private counter for
/// their own thread; `with_limit` hands out arena ids from that counter while
/// it is installed, and every other thread keeps allocating from the real
/// process-global source. This keeps exhaustion deterministic, order-
/// independent, and parallel-safe, and never mutates the real global
/// allocator.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::cell::Cell;
    use std::sync::atomic::AtomicU64;

    thread_local! {
        static ARENA_SOURCE: Cell<Option<&'static AtomicU64>> = const { Cell::new(None) };
    }

    /// The arena-id source installed for the current thread, if any.
    pub(crate) fn source() -> Option<&'static AtomicU64> {
        ARENA_SOURCE.with(|cell| cell.get())
    }

    /// RAII guard installing `counter` as this thread's arena-id source for
    /// the duration of the guard. Restores the previous source on drop.
    ///
    /// Kept as a test seam for a deterministic arena-exhaustion test. No
    /// current de-scoped test constructs it (the process-global counter cannot
    /// be exhausted in practice), so it is allowed dead in the test build.
    #[allow(dead_code)]
    pub(crate) struct ScopedArenaSource;

    #[allow(dead_code)]
    impl ScopedArenaSource {
        pub(crate) fn install(counter: &'static AtomicU64) -> Self {
            ARENA_SOURCE.with(|cell| {
                assert!(
                    cell.get().is_none(),
                    "nested arena source override is unsupported"
                );
                cell.set(Some(counter));
            });
            Self
        }
    }

    #[allow(dead_code)]
    impl Drop for ScopedArenaSource {
        fn drop(&mut self) {
            ARENA_SOURCE.with(|cell| cell.set(None));
        }
    }
}

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
    generation: Cell<u32>,
    /// Concrete type of the current occupant; borrow-time validation only.
    type_id: TypeId,
    /// The resource state is independently guarded so distinct frame requests
    /// may hold disjoint borrows without an aliased `&mut ResourceTable`.
    state: RefCell<SlotState>,
}

/// Cumulative state persisted across [`ResourceTable::poll_close_all`] polls
/// until the table is quiescent.
struct CloseAllState {
    reason: ResourceCloseReason,
    closed: usize,
    /// Total number of cleanup failures observed across the sweep.
    failed: usize,
    first_error: Option<ResourceError>,
}

/// Terminal report of one fully-driven close-all sweep.
///
/// Returned once the table is quiescent; carries the cumulative closed count,
/// the total failure count, and the first (earliest) cleanup failure, so the
/// caller can size the blast radius instead of only seeing one error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseAllReport {
    /// Cumulative number of resources closed across the whole sweep.
    pub closed: usize,
    /// Total number of cleanup failures observed (begin and poll closes),
    /// including the one in `first_error`.
    pub failed: usize,
    /// Earliest cleanup failure observed during the sweep, if any
    /// (first-error-wins).
    pub first_error: Option<ResourceError>,
}

/// Bounded arena of erased resources owned by one execution scope.
///
/// `Send + !Sync` by construction: it must never be shared; the owning scope
/// moves it and mutates it single-threaded.
pub struct ResourceTable {
    arena_id: u64,
    max_entries: usize,
    slots: Vec<ResourceSlot>,
    /// Indices of reusable physical slots. Interior mutability lets the
    /// `&self`-based take path return a consumed slot to the pool immediately.
    vacant_slots: RefCell<Vec<usize>>,
    active_entries: Cell<usize>,
    /// In-flight `poll_close_all` sweep, if one is active.
    close_all: Option<CloseAllState>,
}

/// Hands out the next process-unique arena identity, or a typed
/// [`ResourceErrorCode::ResourceTableArenaExhausted`] once the identity space
/// is exhausted.
///
/// Allocation is atomic and monotonic: the counter is advanced exactly once
/// per successful handout (via `fetch_update`), never on failure, and ids are
/// never recycled or wrapped. Under `#[cfg(test)]`, the current thread's
/// [`test_seam`] override (if installed) replaces the process-global
/// `NEXT_ARENA_ID` so exhaustion tests are deterministic and never consume the
/// real global allocator.
fn allocate_arena_id() -> Result<u64, ResourceError> {
    #[cfg(test)]
    let source = test_seam::source().unwrap_or(&NEXT_ARENA_ID);
    #[cfg(not(test))]
    let source = &NEXT_ARENA_ID;
    source
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |arena_id| {
            (arena_id <= MAX_HANDLE_ARENA_ID).then_some(arena_id + 1)
        })
        .map_err(|_| {
            ResourceError::new(
                ResourceErrorCode::ResourceTableArenaExhausted,
                "resource::table",
                "resource table arena identity space is exhausted",
            )
        })
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
        let arena_id = allocate_arena_id()?;
        Ok(Self {
            arena_id,
            max_entries,
            slots: Vec::new(),
            vacant_slots: RefCell::new(Vec::new()),
            active_entries: Cell::new(0),
            close_all: None,
        })
    }

    /// Creates a table with the default [`DEFAULT_MAX_RESOURCES`] capacity.
    ///
    /// Fallible: arena identity allocation can fail with a typed
    /// [`ResourceErrorCode::ResourceTableArenaExhausted`] once the
    /// process-unique arena space is exhausted. Embeddings and pools must
    /// propagate this error instead of panicking.
    pub fn new() -> ResourceResult<Self> {
        Self::with_limit(DEFAULT_MAX_RESOURCES)
    }

    pub fn len(&self) -> usize {
        self.active_entries.get()
    }

    /// Whether the table currently holds no live resources.
    pub fn is_empty(&self) -> bool {
        self.active_entries.get() == 0
    }

    /// Number of physical slot entries ever carved out of the arena.
    ///
    /// Test-only: proves that close/reuse cycles return slots to the vacant
    /// pool instead of growing physical identity usage without bound.
    #[cfg(test)]
    fn slots_len(&self) -> usize {
        self.slots.len()
    }

    /// Inserts a root resource and returns its typed token.
    pub fn push<T: HostResource>(&mut self, value: T) -> ResourceResult<Resource<T>> {
        let handle = self.allocate(value)?;
        Ok(Resource::from_handle(handle))
    }

    /// Validates a raw [`ResourceHandle`] and recovers a typed token.
    ///
    /// This is the only public way to lift an arbitrary raw handle into a
    /// typed [`Resource<T>`]. It rejects the handle if it belongs to a
    /// different table (arena), refers to a stale slot generation, names the
    /// wrong concrete `TypeId`, or points at a resource that is no longer
    /// `Open`:
    ///
    /// - foreign arena → [`ResourceErrorCode::ResourceHandleWrongTable`]
    /// - stale generation → [`ResourceErrorCode::ResourceStale`]
    /// - wrong type → [`ResourceErrorCode::ResourceTypeMismatch`]
    /// - closed/closing → [`ResourceErrorCode::ResourceAlreadyClosed`]
    ///
    /// A rejected recovery is purely read-only: no slot, generation, or type
    /// state is mutated.
    pub fn typed<T: HostResource>(&self, handle: ResourceHandle) -> ResourceResult<Resource<T>> {
        self.validate_active::<T>(handle)?;
        Ok(Resource::from_handle(handle))
    }

    /// Immutably borrows one live resource for the duration of a host call.
    pub fn get<T: HostResource>(
        &self,
        resource: &Resource<T>,
    ) -> ResourceResult<ResourceRef<'_, T>> {
        let handle = resource.handle();
        let slot_index = self.validate_active::<T>(handle)?;
        self.borrow_open_ref(handle, slot_index)
    }

    /// Mutably borrows one live resource for the duration of a host call.
    pub fn get_mut<T: HostResource>(
        &mut self,
        resource: &Resource<T>,
    ) -> ResourceResult<ResourceMut<'_, T>> {
        let handle = resource.handle();
        let slot_index = self.validate_active::<T>(handle)?;
        self.borrow_open_mut(handle, slot_index)
    }

    fn borrow_open_ref<T: HostResource>(
        &self,
        handle: ResourceHandle,
        slot_index: usize,
    ) -> ResourceResult<ResourceRef<'_, T>> {
        let state = self.slots[slot_index]
            .state
            .try_borrow()
            .map_err(|_| resource_borrow_conflict_error(handle))?;
        let value = Ref::map(state, |state| match state {
            SlotState::Open(resource) => (resource.as_ref() as &dyn Any)
                .downcast_ref::<T>()
                .expect("validated resource TypeId must match downcast type"),
            SlotState::Closing(_) | SlotState::Vacant => {
                unreachable!("validated open resource changed state during shared borrow")
            }
        });
        Ok(ResourceRef::new(handle, value))
    }

    fn borrow_open_mut<T: HostResource>(
        &self,
        handle: ResourceHandle,
        slot_index: usize,
    ) -> ResourceResult<ResourceMut<'_, T>> {
        let state = self.slots[slot_index]
            .state
            .try_borrow_mut()
            .map_err(|_| resource_borrow_conflict_error(handle))?;
        let value = RefMut::map(state, |state| match state {
            SlotState::Open(resource) => (resource.as_mut() as &mut dyn Any)
                .downcast_mut::<T>()
                .expect("validated resource TypeId must match downcast type"),
            SlotState::Closing(_) | SlotState::Vacant => {
                unreachable!("validated open resource changed state during mutable borrow")
            }
        });
        Ok(ResourceMut::new(handle, value))
    }

    /// Begins closing a resource.
    ///
    /// Properties:
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
        self.check_type::<T>(slot_index, handle)?;
        self.close_open_slot(slot_index, handle, reason)
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
        self.check_type::<T>(slot_index, handle)?;

        let state = self.replace_slot_state(slot_index, SlotState::Vacant);
        match state {
            SlotState::Closing(mut resource) => match resource.poll_close(cx) {
                Poll::Ready(result) => {
                    self.reclaim(slot_index);
                    Poll::Ready(result)
                }
                Poll::Pending => {
                    self.put_slot_state(slot_index, SlotState::Closing(resource));
                    Poll::Pending
                }
            },
            SlotState::Open(resource) => {
                // Not closing: restore the open resource and report the precise
                // wrong-state error (distinct from an invalid handle).
                self.put_slot_state(slot_index, SlotState::Open(resource));
                Poll::Ready(Err(not_closing_error(handle)))
            }
            SlotState::Vacant => Poll::Ready(Err(already_closed_error(handle))),
        }
    }

    /// Drives a caller-context close of every live resource.
    ///
    /// This is the event-driven close-all: unlike a synchronous sweep it can
    /// wait on genuinely `Pending` resources using the caller's waker. A
    /// cleanup failure does not stop the remaining best-effort closes: every
    /// resource close is attempted and the first failure is retained until the
    /// whole sweep finishes.
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
    pub fn poll_close_all(
        &mut self,
        reason: ResourceCloseReason,
        cx: &mut Context<'_>,
    ) -> Poll<ResourceResult<usize>> {
        match self.poll_close_all_report(reason, cx) {
            Poll::Pending => Poll::Pending,
            // Preserve the legacy error surface: a sweep that finished with
            // cleanup failures reports `Err(first_error)` here, while the
            // report-based variant carries the full failure count.
            Poll::Ready(Ok(report)) => match report.first_error {
                Some(error) => Poll::Ready(Err(error)),
                None => Poll::Ready(Ok(report.closed)),
            },
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }

    /// Drives a caller-context close of every live resource and reports the
    /// full sweep result (closed count, failure count, first failure) exactly
    /// once the table is quiescent.
    ///
    /// Same contract and sweep as [`poll_close_all`](Self::poll_close_all),
    /// but the terminal [`CloseAllReport`] carries the cumulative closed
    /// count, the total failure count, and the earliest failure instead of
    /// only the first error. This is the report the execution scope consumes
    /// so its own terminal outcome can carry the failure count.
    pub fn poll_close_all_report(
        &mut self,
        reason: ResourceCloseReason,
        cx: &mut Context<'_>,
    ) -> Poll<ResourceResult<CloseAllReport>> {
        // Deterministically reject a conflicting reason. The in-flight sweep
        // keeps the reason it started with; we do not mutate any state here.
        if self
            .close_all
            .as_ref()
            .is_some_and(|state| state.reason != reason)
        {
            let in_progress = self.close_all.as_ref().expect("checked above").reason;
            return Poll::Ready(Err(close_in_progress_error(reason, in_progress)));
        }
        if self.close_all.is_none() {
            self.close_all = Some(CloseAllState {
                reason,
                closed: 0,
                failed: 0,
                first_error: None,
            });
        }
        let reason = self.close_all.as_ref().unwrap().reason;
        let mut closed = self.close_all.as_ref().unwrap().closed;
        let mut failed = self.close_all.as_ref().unwrap().failed;
        let mut first_error = self.close_all.as_ref().unwrap().first_error.clone();

        // Sweep until a full pass makes no progress: every current open
        // resource is begun, every Closing resource is polled, and both repeat
        // until the state stabilizes. Genuinely-Pending resources stay in
        // `Closing` and are re-polled on a later `poll_close_all` call with the
        // real waker.
        let mut progressed = true;
        while progressed {
            progressed = false;
            let open_indices = self.open_indices()?;
            for slot_index in open_indices {
                progressed |= self.try_begin_close(
                    slot_index,
                    reason,
                    &mut closed,
                    &mut failed,
                    &mut first_error,
                );
            }
            let closing_indices = self.closing_indices()?;
            for slot_index in closing_indices {
                progressed |=
                    self.try_poll_close(slot_index, cx, &mut closed, &mut failed, &mut first_error);
            }
        }

        // Persist cumulative progress across Pending polls.
        let state = self.close_all.as_mut().unwrap();
        state.closed = closed;
        state.failed = failed;
        state.first_error = first_error;

        if self.is_empty() {
            // Quiescent: this, and only this, warrants a Ready completion.
            let state = self.close_all.take().unwrap();
            Poll::Ready(Ok(CloseAllReport {
                closed: state.closed,
                failed: state.failed,
                first_error: state.first_error,
            }))
        } else {
            Poll::Pending
        }
    }

    /// Drop-only, nonblocking close launch for every remaining open resource.
    ///
    /// Unlike the reusable close/reset sweep, this phase does not wait for a
    /// pending resource to become quiescent before continuing. It invokes
    /// `begin_close` once for each still-open slot, retains closing slots in
    /// `Closing`, and never reports table quiescence. Already-closing slots are
    /// left untouched, preserving exactly-once begin semantics.
    pub(crate) fn begin_close_remaining_for_drop(
        &mut self,
        reason: ResourceCloseReason,
    ) -> ResourceResult<()> {
        let indices = self.live_indices()?;
        let mut first_error = None;

        for slot_index in indices {
            let state = self.replace_slot_state(slot_index, SlotState::Vacant);
            let SlotState::Open(mut resource) = state else {
                self.put_slot_state(slot_index, state);
                continue;
            };
            match resource.begin_close(reason) {
                Ok(CloseProgress::Ready) => self.reclaim(slot_index),
                Ok(CloseProgress::Pending) => {
                    self.put_slot_state(slot_index, SlotState::Closing(resource));
                }
                Err(error) => {
                    self.put_slot_state(slot_index, SlotState::Open(resource));
                    first_error.get_or_insert(error);
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
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
    /// - [`ResourceErrorCode::ResourceClosePending`] is returned (never
    ///   success) when at least one resource remains pending at the end of the
    ///   single no-op sweep, because such a resource needs an external waker
    ///   that a synchronous no-op driver cannot provide.
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

    fn replace_slot_state(&mut self, slot_index: usize, state: SlotState) -> SlotState {
        std::mem::replace(self.slots[slot_index].state.get_mut(), state)
    }

    fn put_slot_state(&mut self, slot_index: usize, state: SlotState) {
        *self.slots[slot_index].state.get_mut() = state;
    }

    fn close_open_slot(
        &mut self,
        slot_index: usize,
        handle: ResourceHandle,
        reason: ResourceCloseReason,
    ) -> ResourceResult<CloseProgress> {
        let state = self.replace_slot_state(slot_index, SlotState::Vacant);
        match state {
            SlotState::Open(mut resource) => match resource.begin_close(reason) {
                Ok(CloseProgress::Ready) => {
                    self.reclaim(slot_index);
                    Ok(CloseProgress::Ready)
                }
                Ok(CloseProgress::Pending) => {
                    self.put_slot_state(slot_index, SlotState::Closing(resource));
                    Ok(CloseProgress::Pending)
                }
                Err(error) => {
                    // Explicit-close failure stays local: the resource is
                    // left Open so a later shutdown sweep retries the
                    // idempotent close request. The failure is returned to
                    // the caller (which records it in the scope latch);
                    // the resource is NOT dropped or reclaimed here.
                    self.put_slot_state(slot_index, SlotState::Open(resource));
                    Err(error)
                }
            },
            SlotState::Closing(resource) => {
                // Idempotent: the close is already in flight; keep holding the
                // generation until the outer caller drives poll_close.
                self.put_slot_state(slot_index, SlotState::Closing(resource));
                Ok(CloseProgress::Pending)
            }
            SlotState::Vacant => Err(already_closed_error(handle)),
        }
    }

    fn try_begin_close(
        &mut self,
        slot_index: usize,
        reason: ResourceCloseReason,
        closed: &mut usize,
        failed: &mut usize,
        first_error: &mut Option<ResourceError>,
    ) -> bool {
        let state = self.replace_slot_state(slot_index, SlotState::Vacant);
        let SlotState::Open(mut resource) = state else {
            // Not open (e.g. already closing); restore and report no progress.
            self.put_slot_state(slot_index, state);
            return false;
        };
        match resource.begin_close(reason) {
            Ok(CloseProgress::Ready) => {
                self.reclaim(slot_index);
                *closed += 1;
                true
            }
            Ok(CloseProgress::Pending) => {
                self.put_slot_state(slot_index, SlotState::Closing(resource));
                true
            }
            Err(error) => {
                self.reclaim(slot_index);
                *closed += 1;
                *failed += 1;
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
        failed: &mut usize,
        first_error: &mut Option<ResourceError>,
    ) -> bool {
        let state = self.replace_slot_state(slot_index, SlotState::Vacant);
        let SlotState::Closing(mut resource) = state else {
            self.put_slot_state(slot_index, state);
            return false;
        };
        match resource.poll_close(cx) {
            Poll::Ready(result) => {
                self.reclaim(slot_index);
                *closed += 1;
                if let Err(error) = result {
                    *failed += 1;
                    first_error.get_or_insert(error);
                }
                true
            }
            Poll::Pending => {
                self.put_slot_state(slot_index, SlotState::Closing(resource));
                false
            }
        }
    }

    fn reclaim(&mut self, slot_index: usize) {
        self.put_slot_state(slot_index, SlotState::Vacant);
        if u64::from(self.slots[slot_index].generation.get()) < MAX_HANDLE_GENERATION {
            self.vacant_slots.get_mut().push(slot_index);
        }
        self.active_entries.set(self.active_entries.get() - 1);
    }

    /// Indices of slots currently in [`SlotState::Open`].
    fn open_indices(&self) -> ResourceResult<Vec<usize>> {
        let mut indices = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            let state = slot
                .state
                .try_borrow()
                .map_err(|_| resource_borrow_conflict_error_for_slot(slot))?;
            if matches!(&*state, SlotState::Open(_)) {
                indices.push(index);
            }
        }
        Ok(indices)
    }

    /// Indices of slots currently in [`SlotState::Closing`].
    fn closing_indices(&self) -> ResourceResult<Vec<usize>> {
        let mut indices = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            let state = slot
                .state
                .try_borrow()
                .map_err(|_| resource_borrow_conflict_error_for_slot(slot))?;
            if matches!(&*state, SlotState::Closing(_)) {
                indices.push(index);
            }
        }
        Ok(indices)
    }

    fn live_indices(&mut self) -> ResourceResult<Vec<usize>> {
        let mut indices = Vec::new();
        for slot_index in 0..self.slots.len() {
            if !matches!(self.slots[slot_index].state.get_mut(), SlotState::Vacant) {
                indices.push(slot_index);
            }
        }
        Ok(indices)
    }

    // ---- allocation ---------------------------------------------------------------

    fn allocate<T: HostResource>(&mut self, value: T) -> Result<ResourceHandle, ResourceError> {
        if self.active_entries.get() >= self.max_entries {
            return Err(ResourceError::new(
                ResourceErrorCode::ResourceLimitExceeded,
                "resource::push",
                "resource table capacity has been reached",
            )
            .with_limit(self.max_entries));
        }

        let type_id = TypeId::of::<T>();
        let value: Box<dyn HostResource> = Box::new(value);

        let (slot_index, generation) = if let Some(slot_index) = self.vacant_slots.get_mut().pop() {
            let generation = self.slots[slot_index]
                .generation
                .get()
                .checked_add(1)
                .filter(|generation| u64::from(*generation) <= MAX_HANDLE_GENERATION)
                .expect("only reusable generations enter the vacant list");
            self.slots[slot_index].generation.set(generation);
            self.slots[slot_index].type_id = type_id;
            *self.slots[slot_index].state.get_mut() = SlotState::Open(value);
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
                generation: Cell::new(generation),
                type_id,
                state: RefCell::new(SlotState::Open(value)),
            });
            (slot_index, generation)
        };
        self.active_entries.set(self.active_entries.get() + 1);
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
        if u64::from(self.slots[slot_index].generation.get()) != handle.generation() {
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
        let state = self.slots[slot_index]
            .state
            .try_borrow()
            .map_err(|_| resource_borrow_conflict_error(handle))?;
        if !matches!(&*state, SlotState::Open(_)) {
            return Err(already_closed_error(handle));
        }
        Ok(slot_index)
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

fn resource_borrow_conflict_error(handle: ResourceHandle) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceAccessConflict,
        "resource::access",
        "resource slot is already borrowed",
    )
    .with_value(handle.raw())
}

fn resource_borrow_conflict_error_for_slot(_slot: &ResourceSlot) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceAccessConflict,
        "resource::access",
        "resource slot is already borrowed",
    )
}

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

    const REASON: ResourceCloseReason = ResourceCloseReason::ResourceClosed;

    /// A resource that counts synchronous closes.
    #[derive(Debug)]
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

    #[test]
    fn typed_recovery_and_borrow_validate_type_and_state() {
        let mut table = ResourceTable::new().expect("table");
        let (res, closes) = UnitRes::new();
        let token = table.push(res).unwrap();

        // Public validated recovery returns an equivalent token.
        let recovered = table.typed::<UnitRes>(token.handle()).expect("recovery");
        assert_eq!(recovered.handle(), token.handle());
        table.get(&recovered).expect("recovered token borrows");

        // The crate-private constructor is only reachable inside this crate;
        // constructing a mismatched token here exercises rejection logic.
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
    fn begin_close_is_exact_once_and_stales_the_handle() {
        let mut table = ResourceTable::new().expect("table");
        let (res, closes) = UnitRes::new();
        let token = table.push(res).unwrap();
        table
            .begin_close(token, REASON)
            .expect("first close succeeds");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        // A second close of the same token is already-closed.
        assert_eq!(
            table
                .begin_close(token, REASON)
                .expect_err("second close rejected")
                .code(),
            ResourceErrorCode::ResourceAlreadyClosed
        );
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn stale_and_foreign_handles_are_rejected_with_typed_errors() {
        let mut table = ResourceTable::new().expect("table");
        let (res, _) = UnitRes::new();
        let token = table.push(res).unwrap();
        let handle = token.handle();
        table.begin_close(token, REASON).unwrap();

        // Immediately after a close the live generation is vacant: the same
        // handle reports AlreadyClosed (precise closed-state error).
        assert_eq!(
            table.typed::<UnitRes>(handle).expect_err("closed").code(),
            ResourceErrorCode::ResourceAlreadyClosed
        );
        // Reusing the slot advances its generation, so the old closed handle
        // becomes a normal stale handle.
        let _reused = table.push(UnitRes::new().0).unwrap();
        assert_eq!(
            table.typed::<UnitRes>(handle).expect_err("stale").code(),
            ResourceErrorCode::ResourceStale
        );
        // Foreign arena.
        let other = ResourceTable::new().expect("other table");
        assert_eq!(
            other.typed::<UnitRes>(handle).expect_err("foreign").code(),
            ResourceErrorCode::ResourceHandleWrongTable
        );
    }

    #[test]
    fn table_capacity_is_bounded_and_close_restores_it() {
        let mut table = ResourceTable::with_limit(2).expect("table");
        let (a, _) = UnitRes::new();
        let (b, _) = UnitRes::new();
        table.push(a).unwrap();
        table.push(b).unwrap();
        let (c, _) = UnitRes::new();
        let error = table.push(c).expect_err("capacity reached");
        assert_eq!(error.code(), ResourceErrorCode::ResourceLimitExceeded);

        // Closing a resource restores capacity (slot reused).
        table.close_all(REASON).expect("close all");
        assert_eq!(table.len(), 0);
        // Reuse stays bounded: many close/re-push cycles never exceed the
        // physical slot arena nor the configured capacity.
        for _ in 0..4 {
            let (res, _) = UnitRes::new();
            let token = table.push(res).expect("re-push after close");
            let _ = table.begin_close(token, REASON).expect("begin_close");
        }
        assert_eq!(table.len(), 0);
        assert!(
            table.slots_len() <= 2,
            "slot arena must stay bounded by the configured capacity"
        );
    }
}
