//! Operation registry: slot lifecycle, bounds, deadline and first-reason
//! cancellation tracking for host-agnostic operations.
//!
//! The registry owns a bounded, reusable generational slot arena. Each
//! occupied slot owns an object-safe [`HostOperation`] driver plus an
//! optional deadline, resource association, cleanup and its own status.
//! Packed `tag`/`slot`/`generation` ids are fully validated against the
//! live slot descriptor before any mutation, so a foreign-tagged, stale or
//! out-of-range id is rejected rather than aliased to a newer occupant.
//!
//! Cancellation is first-reason-wins, recorded once, and forwarded only to
//! the owning concrete driver via [`HostOperation::cancel`]. There is no
//! host-domain dispatch and no secondary cancellation channel.

use std::task::{Context, Poll};
use std::time::Instant;

use super::driver::{HostOperation, OperationCleanup, OperationOutcome, OperationSpec};
use super::error::{OperationError, OperationErrorCode, OperationResult};
use super::id::{MAX_GENERATION, MAX_SLOT_IDENTITY, OperationId, allocate_registry_tag, encode};
use super::reason::OperationCancelReason;
use crate::vm::resource::ResourceHandle;

/// Default ceiling for concurrently pending operations.
pub const DEFAULT_MAX_PENDING_OPERATIONS: usize = 64;

/// Public, observable operation status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    /// Still running.
    Pending,
    /// Finished successfully.
    Completed,
    /// Cancelled; carries the first cancellation reason.
    Cancelled(OperationCancelReason),
    /// Failed with an operation error.
    Failed(OperationError),
}

impl OperationStatus {
    /// Whether the operation has reached a terminal (non-pending) state.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, OperationStatus::Pending)
    }

    fn terminal_outcome(&self) -> Option<OperationOutcome> {
        match self {
            OperationStatus::Pending => None,
            OperationStatus::Completed => Some(OperationOutcome::Completed),
            OperationStatus::Cancelled(reason) => Some(OperationOutcome::Cancelled(*reason)),
            OperationStatus::Failed(error) => Some(OperationOutcome::Failed(error.clone())),
        }
    }
}

/// One generational slot in the registry's slot arena.
///
/// A slot keeps a nonzero generation across reuses; each new occupant of the
/// same slot sees an incremented generation, so an id from a previous occupant
/// becomes stale rather than aliasing a newer operation.
struct OperationSlot {
    generation: u64,
    operation: Option<Operation>,
}

struct Operation {
    driver: Box<dyn HostOperation>,
    deadline: Option<Instant>,
    resource: Option<ResourceHandle>,
    cleanup: Option<OperationCleanup>,
    status: OperationStatus,
}

/// Reusable, slot-arena registry of in-flight host operations.
///
/// Capacity limits the number of *pending* operations; an operation that has
/// reached a terminal state no longer counts against capacity, so consuming a
/// terminal result releases registry capacity for new operations.
///
/// Storage is a [`Vec<OperationSlot>`] backed by a free list of reusable slot
/// indices. Each operation id packs the registry's process-unique tag, the
/// slot identity, and the slot's generation, so a caller-supplied id that
/// carries another registry's tag, an out-of-range/future slot, or a stale
/// generation is rejected before any status, driver, cleanup or free-list
/// mutation.
///
/// This type is intentionally `!Sync` (no interior mutability for concurrent
/// access); it is owned and driven by a single thread.
pub struct OperationRegistry {
    max_pending: usize,
    tag: u64,
    sealed: bool,
    slots: Vec<OperationSlot>,
    free: Vec<usize>,
}

impl OperationRegistry {
    /// Creates an empty registry with the default pending-operation ceiling.
    ///
    /// Tag allocation is process-unique and fallible; callers must propagate
    /// [`OperationErrorCode::OperationRegistryTagExhausted`] rather than rely
    /// on an infallible default constructor.
    pub fn new() -> OperationResult<Self> {
        Self::with_limit(DEFAULT_MAX_PENDING_OPERATIONS)
    }

    /// Creates an empty sealed-less registry with the given pending-operation
    /// ceiling, allocating a process-unique registry tag.
    pub fn with_limit(max_pending: usize) -> OperationResult<Self> {
        if max_pending == 0 {
            return Err(OperationError::new(
                OperationErrorCode::InvalidConfiguration,
                "vm::operation",
                "operation registry capacity must be positive",
            ));
        }
        let tag = allocate_registry_tag()?;
        Ok(Self {
            max_pending,
            tag,
            sealed: false,
            slots: Vec::new(),
            free: Vec::new(),
        })
    }

    /// The configured pending-operation ceiling.
    pub fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// Whether this registry has been [`seal`](Self::seal)ed and therefore
    /// rejects new operations.
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Seals the registry so no further operations can be started. Idempotent;
    /// existing operations remain queryable and droppable.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// Number of operations still pending.
    pub fn active_count(&self) -> usize {
        self.slots
            .iter()
            .filter_map(|slot| slot.operation.as_ref())
            .filter(|operation| !operation.status.is_terminal())
            .count()
    }

    /// Number of occupied slots (pending and terminal).
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.operation.is_some()).count()
    }

    /// Whether no operation (pending or terminal) is occupied.
    pub fn is_empty(&self) -> bool {
        !self.slots.iter().any(|s| s.operation.is_some())
    }

    /// Starts a new operation from a spec, enforcing the seal, the capacity
    /// ceiling, generic slot reuse, and packed id allocation.
    pub fn start(&mut self, spec: OperationSpec) -> OperationResult<OperationId> {
        if self.sealed {
            return Err(OperationError::new(
                OperationErrorCode::OperationRegistrySealed,
                "vm::operation",
                "operation registry is sealed and rejects new operations",
            ));
        }
        if self.active_count() >= self.max_pending {
            return Err(OperationError::new(
                OperationErrorCode::OperationLimitExceeded,
                "vm::operation",
                "pending operation capacity has been reached",
            )
            .with_limit(self.max_pending as u64));
        }
        let slot_index = self.acquire_slot()?;
        let generation = self.slots[slot_index].generation;
        let id = encode(self.tag, slot_index, generation).expect("registry id encodes");
        let operation = Operation {
            driver: spec.driver,
            deadline: spec.deadline,
            resource: spec.resource,
            cleanup: spec.cleanup,
            status: OperationStatus::Pending,
        };
        // Install exactly once into the acquired slot.
        self.slots[slot_index].operation = Some(operation);
        debug_assert!(self.slots[slot_index].generation == generation);
        Ok(id)
    }

    /// Observes the current status of an operation.
    pub fn status(&self, id: OperationId) -> OperationResult<OperationStatus> {
        Ok(self.operation(id)?.status.clone())
    }

    /// Consumes the terminal outcome of an operation, delivering it exactly
    /// once and immediately releasing its slot for reuse under an incremented
    /// generation. After this call the id is stale.
    ///
    /// A pending operation returns `OperationPending` without mutating the
    /// registry; drive it to terminal with `poll` first.
    pub fn take_outcome(&mut self, id: OperationId) -> OperationResult<OperationOutcome> {
        let slot = self.location(id)?;
        let status = self.slots[slot]
            .operation
            .as_ref()
            .map(|operation| operation.status.clone())
            .ok_or_else(|| operation_stale(id))?;
        let outcome = status
            .terminal_outcome()
            .ok_or_else(|| pending_outcome(id))?;
        self.release_slot(slot);
        Ok(outcome)
    }

    /// The resource handle an operation is associated with, if any.
    pub fn resource_of(&self, id: OperationId) -> OperationResult<Option<ResourceHandle>> {
        Ok(self.operation(id)?.resource)
    }

    /// Ids of operations associated with the given resource handle.
    pub fn operations_for_resource(&self, resource: ResourceHandle) -> Vec<OperationId> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let operation = slot.operation.as_ref()?;
                if operation.resource == Some(resource) {
                    Some(self.id_at(index, slot.generation))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Drives the operation one step.
    ///
    /// Polls the owning driver first; a `Ready` driver result wins even if a
    /// deadline has already elapsed. Only a pending driver result falls
    /// through to the deadline check, in which case an elapsed deadline
    /// cancels the operation with `OperationCancelReason::Deadline`.
    ///
    /// The terminal outcome is delivered exactly once: when this returns
    /// `Poll::Ready`, the operation's slot is released immediately and the id
    /// becomes stale. A later `poll`, `status` or `take_outcome` on that id
    /// returns `OperationStale`. An out-of-band terminal (`complete`, `fail`
    /// or `cancel`) left on the entry is consumed here on the next `poll`.
    pub fn poll(
        &mut self,
        id: OperationId,
        cx: &mut Context<'_>,
    ) -> Poll<OperationResult<OperationOutcome>> {
        // Validate fully before any mutation.
        let slot = match self.location(id) {
            Ok(slot) => slot,
            Err(error) => return Poll::Ready(Err(error)),
        };

        // An out-of-band terminal (complete/fail/cancel) is consumed one-shot.
        if self.slots[slot]
            .operation
            .as_ref()
            .is_some_and(|operation| operation.status.is_terminal())
        {
            return Poll::Ready(Ok(self.consume_terminal(slot)));
        }

        // Drive the real driver first; a Ready result wins even if a deadline
        // has already elapsed.
        let driver_result = {
            let operation = self.slots[slot].operation.as_mut().expect("slot occupied");
            operation.driver.poll(cx)
        };
        match driver_result {
            Poll::Pending => {
                // Only a pending driver result falls through to the deadline.
                let deadline_elapsed = self.slots[slot]
                    .operation
                    .as_ref()
                    .and_then(|operation| operation.deadline)
                    .is_some_and(|deadline| Instant::now() >= deadline);
                if !deadline_elapsed {
                    return Poll::Pending;
                }
                // An elapsed deadline cancels; the resulting terminal state is
                // then consumed one-shot.
                let _ = self.cancel(id, OperationCancelReason::Deadline);
                let slot = match self.location(id) {
                    Ok(slot) => slot,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                Poll::Ready(Ok(self.consume_terminal(slot)))
            }
            Poll::Ready(Ok(())) => {
                // Success beats an elapsed deadline.
                let _ = self.finish_terminal(
                    id,
                    OperationStatus::Completed,
                    OperationOutcome::Completed,
                );
                let slot = match self.location(id) {
                    Ok(slot) => slot,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                Poll::Ready(Ok(self.consume_terminal(slot)))
            }
            Poll::Ready(Err(error)) => {
                // A driver failure beats an elapsed deadline.
                let _ = self.finish_terminal(
                    id,
                    OperationStatus::Failed(error.clone()),
                    OperationOutcome::Failed(error),
                );
                let slot = match self.location(id) {
                    Ok(slot) => slot,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                Poll::Ready(Ok(self.consume_terminal(slot)))
            }
        }
    }

    /// Cancels one operation, forwarding the reason to its driver.
    ///
    /// The id is validated before any mutation, and the driver's
    /// [`HostOperation::cancel`] is invoked while the operation is still
    /// `Pending`. On success the operation finishes as `Cancelled` through the
    /// central cleanup helper. An already-terminal operation returns
    /// `Ok(false)` and preserves its first recorded reason; the driver is not
    /// invoked again.
    ///
    /// A driver cancel failure is wrapped as `OperationDriverFailed`: the
    /// terminal status becomes `Failed(first)`, the cleanup runs once with
    /// that `Failed` outcome, and the driver error is returned (preserved as
    /// the first error even if cleanup also fails). No false `Cancelled` state
    /// is produced.
    pub fn cancel(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> OperationResult<bool> {
        let slot = self.location(id)?;
        let pending = self.slots[slot]
            .operation
            .as_ref()
            .is_some_and(|operation| matches!(operation.status, OperationStatus::Pending));
        if !pending {
            return Ok(false);
        }

        // Call the driver while still pending, before recording any status.
        let driver_result = {
            let operation = self.slots[slot].operation.as_mut().expect("pending above");
            operation.driver.cancel(reason)
        };
        match driver_result {
            Ok(()) => {
                // Finish as Cancelled through the central cleanup helper.
                self.finish_terminal(
                    id,
                    OperationStatus::Cancelled(reason),
                    OperationOutcome::Cancelled(reason),
                )
                .map(|_| true)
            }
            Err(error) => {
                // The driver failed to cancel: record Failed(first) and run the
                // cleanup once with that outcome. The driver error stays first
                // even if cleanup also fails.
                let first = driver_failure(error);
                let cleanup = {
                    let operation = self.slots[slot].operation.as_mut().expect("pending above");
                    operation.status = OperationStatus::Failed(first.clone());
                    operation.cleanup.take()
                };
                if let Some(cleanup) = cleanup {
                    let _ = cleanup(&OperationOutcome::Failed(first.clone()));
                }
                Err(first)
            }
        }
    }

    /// Aborts a started operation that must never produce a guest-visible
    /// result: cancels the driver exactly once if it is still pending, then
    /// consumes/immediately releases the slot so the id becomes stale and
    /// full registry capacity is restored (the same "cancel then consume"
    /// sequence the batch drain helpers use).
    ///
    /// This is the rollback counterpart to [`start`](Self::start), for call
    /// sites that register an operation and then hit a fallible handoff (for
    /// example a bridge submission) before installing the pending-result
    /// adapter. Without it, a failed handoff would leave a registered
    /// terminal or pending entry occupying registry capacity until some later
    /// `poll`/`take_outcome`/reset.
    ///
    /// - **Pending** — the driver is cancelled exactly once with `reason`
    ///   (first-reason-wins), the resulting terminal outcome is consumed and
    ///   the slot released, and `Ok(true)` is returned. If the driver's
    ///   ``cancel`` itself fails, that failure is recorded as the first
    ///   `Failed` status (possibly `Failed(OperationDriverFailed)` when the
    ///   driver surfaces a typed poison/error), the cleanup runs once, the
    ///   slot is still released, and the driver error is returned so the
    ///   caller can preserve it as the first reason — the slot is never left
    ///   occupied regardless of the cancel outcome.
    /// - **Already terminal** — the terminal outcome is consumed, the slot
    ///   released, and `Ok(false)` returned (the driver is not invoked again).
    /// - **Stale / foreign / out-of-range** — rejected with the usual typed
    ///   error and **no** registry mutation.
    ///
    /// After a successful abort the id is stale under an incremented slot
    /// generation, so a later `poll`, `status`, `take_outcome`, `remove` or
    /// second `abort` on it all report `OperationStale`.
    pub fn abort(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> OperationResult<bool> {
        // Validate fully before any mutation; an unresolvable id is rejected
        // without touching cancel/consume state.
        let _ = self.location(id)?;
        let cancel_result = self.cancel(id, reason);
        // Whether the driver cancelled cleanly, the driver's cancel failed
        // (the entry is now terminal `Failed`), or the entry was already
        // terminal before this call, consuming the outcome releases the slot
        // and makes the id stale exactly once.
        let _ = self.take_outcome(id);
        cancel_result
    }

    /// Cancels every pending operation associated with `resource` and drains
    /// every matching terminal slot, returning an
    /// [`OperationCancelSummary`].
    ///
    /// Snapshots every occupied operation matching that exact
    /// [`ResourceHandle`] (pending and pre-existing terminal) in ascending
    /// slot order. For each snapshot: if it is still pending, it is cancelled
    /// exactly once and the resulting terminal outcome is consumed and its
    /// slot released, counting toward the summary (every attempted pending
    /// operation increments `matched`, only a successful `Cancelled`
    /// increments `cancelled`, and a driver/cleanup failure increments
    /// `failed` with the first error stored). If it was already terminal
    /// before this call, its outcome is consumed and its slot released
    /// without counting toward the summary. Failures are isolated; every
    /// matching pending operation is still attempted. Nonmatching slots are
    /// left untouched.
    pub fn cancel_for_resource(
        &mut self,
        resource: ResourceHandle,
        reason: OperationCancelReason,
    ) -> OperationCancelSummary {
        let mut summary = OperationCancelSummary::default();
        for id in self.ids_for_resource(resource) {
            if let Some(result) = self.drain_batch(id, reason) {
                summary.record(result);
            }
        }
        summary
    }

    /// Cancels all pending operations and drains every terminal slot, so the
    /// registry reaches quiescence (`is_empty()` and `len() == 0`).
    ///
    /// Snapshots every occupied slot (pending and pre-existing terminal) in
    /// ascending slot order. For each snapshot id: if it is still pending, it
    /// is cancelled exactly once, the resulting terminal outcome is consumed
    /// and its slot released, and exactly one result is recorded in the
    /// returned [`OperationCancelSummary`] (every attempted pending operation
    /// increments `matched`, only a successful `Cancelled` increments
    /// `cancelled`, and a driver/cleanup failure increments `failed` with the
    /// first error stored). If it was already terminal before this call, its
    /// outcome is consumed and discarded and its slot released without
    /// counting toward the summary. On return every previous id is stale,
    /// including pre-existing terminal operations, and the registry is empty.
    pub fn cancel_all(&mut self, reason: OperationCancelReason) -> OperationCancelSummary {
        let mut summary = OperationCancelSummary::default();
        for id in self.occupied_ids() {
            if let Some(result) = self.drain_batch(id, reason) {
                summary.record(result);
            }
        }
        summary
    }

    /// Bulk-drain helper shared by [`cancel_all`](Self::cancel_all) and
    /// [`cancel_for_resource`](Self::cancel_for_resource).
    ///
    /// If the snapshot id is still pending, it is cancelled exactly once, the
    /// resulting terminal outcome is consumed and its slot released through
    /// [`take_outcome`](Self::take_outcome) (so generation/free-list updates
    /// happen exactly once), and `Some(result)` is returned for the caller to
    /// record in an [`OperationCancelSummary`]. If `id` was already terminal
    /// before the snapshot, its outcome is consumed and discarded, its slot
    /// released, and `None` is returned so the caller does not count a
    /// matched/cancelled/failed increment. A pending slot is never released
    /// without cancelling: `take_outcome` refuses to release an id that is
    /// (impossibly) still pending after a successful cancellation.
    fn drain_batch(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> Option<OperationResult<bool>> {
        let is_pending = self
            .location(id)
            .ok()
            .and_then(|slot| self.slots[slot].operation.as_ref())
            .is_some_and(|operation| matches!(operation.status, OperationStatus::Pending));
        if is_pending {
            let result = self.cancel(id, reason);
            let _ = self.take_outcome(id);
            Some(result)
        } else {
            // Pre-existing terminal (or an unresolvable id): consume and
            // discard its outcome, releasing the slot without recording a
            // matched/cancelled/failed increment.
            let _ = self.take_outcome(id);
            None
        }
    }

    /// Marks an operation completed out-of-band (e.g. a host future resolved
    /// without a poll). The result stays terminal until
    /// [`take_outcome`](Self::take_outcome) or [`remove`](Self::remove) is
    /// called. Returns `Ok(false)` if already terminal; a cleanup failure
    /// returns `Err` while the status becomes `Failed`.
    pub fn complete(&mut self, id: OperationId) -> OperationResult<bool> {
        self.finish_terminal(id, OperationStatus::Completed, OperationOutcome::Completed)
    }

    /// Marks an operation failed out-of-band. The result stays terminal until
    /// [`take_outcome`](Self::take_outcome) or [`remove`](Self::remove) is
    /// called. Returns `Ok(false)` if already terminal; a cleanup failure
    /// returns `Err` while the status becomes `Failed`.
    pub fn fail(&mut self, id: OperationId, error: OperationError) -> OperationResult<bool> {
        self.finish_terminal(
            id,
            OperationStatus::Failed(error.clone()),
            OperationOutcome::Failed(error),
        )
    }

    /// Removes a single operation, returning its status and releasing its slot
    /// for reuse.
    ///
    /// This is an explicit *terminal-state* discard: only an already-terminal
    /// operation is removed and its slot released. A still-`Pending`
    /// operation returns `OperationPending` and is left completely untouched —
    /// its driver is not cancelled, no cleanup runs, and its slot generation
    /// and free-list membership are unchanged. Drive a task with
    /// [`poll`](Self::poll) (or [`cancel`](Self::cancel)) to reach a terminal
    /// state before removing it.
    pub fn remove(&mut self, id: OperationId) -> OperationResult<OperationStatus> {
        let index = self.location(id)?;
        let terminal = self.slots[index]
            .operation
            .as_ref()
            .is_some_and(|operation| operation.status.is_terminal());
        if !terminal {
            return Err(pending_outcome(id));
        }
        let status = {
            let slot = &mut self.slots[index];
            match slot.operation.take() {
                Some(operation) => operation.status,
                None => return Err(operation_stale(id)),
            }
        };
        self.release_slot(index);
        Ok(status)
    }

    /// Installs a requested terminal status and runs the (once) cleanup hook.
    /// No-op (returns `Ok(false)`) when the operation is already terminal.
    ///
    /// A cleanup failure is wrapped as `OperationCleanupFailed`, replaces the
    /// terminal status with `Failed(wrapped)`, leaves the operation terminal,
    /// and returns the wrapped error.
    fn finish_terminal(
        &mut self,
        id: OperationId,
        status: OperationStatus,
        outcome: OperationOutcome,
    ) -> OperationResult<bool> {
        let slot = self.location(id)?;
        let cleanup = {
            let operation = match self.slots[slot].operation.as_mut() {
                Some(operation) => operation,
                None => return Ok(false),
            };
            if operation.status.is_terminal() {
                return Ok(false);
            }
            operation.status = status;
            operation.cleanup.take()
        };
        if let Some(cleanup) = cleanup {
            self.run_cleanup(slot, cleanup, outcome)?;
        }
        Ok(true)
    }

    /// Runs an already-taken cleanup exactly once with the terminal outcome.
    /// A failure wraps the error as `OperationCleanupFailed`, overrides the
    /// operation's status to `Failed(wrapped)`, and returns the wrapped error.
    fn run_cleanup(
        &mut self,
        slot: usize,
        cleanup: OperationCleanup,
        outcome: OperationOutcome,
    ) -> OperationResult<()> {
        match cleanup(&outcome) {
            Ok(()) => Ok(()),
            Err(error) => {
                let wrapped = OperationError::new(
                    OperationErrorCode::OperationCleanupFailed,
                    "vm::operation",
                    error.to_string(),
                );
                if let Some(operation) = self.slots[slot].operation.as_mut() {
                    operation.status = OperationStatus::Failed(wrapped.clone());
                }
                Err(wrapped)
            }
        }
    }

    /// Reads and releases a terminal slot in one step, delivering its outcome.
    /// Caller must have validated an occupied terminal slot.
    fn consume_terminal(&mut self, slot: usize) -> OperationOutcome {
        let status = self.slots[slot]
            .operation
            .as_ref()
            .expect("terminal slot remains occupied")
            .status
            .clone();
        let outcome = status
            .terminal_outcome()
            .expect("terminal status has an outcome");
        self.release_slot(slot);
        outcome
    }

    /// Ids of every occupied slot (pending and terminal), in ascending slot
    /// order. Used by [`cancel_all`](Self::cancel_all) to snapshot all
    /// occupants before draining.
    fn occupied_ids(&self) -> Vec<OperationId> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.operation
                    .as_ref()
                    .map(|_| self.id_at(index, slot.generation))
            })
            .collect()
    }

    /// Ids of every occupied slot (pending and terminal) associated with
    /// exactly `resource`, in ascending slot order. Used by
    /// [`cancel_for_resource`](Self::cancel_for_resource) to snapshot all
    /// matching occupants before draining.
    fn ids_for_resource(&self, resource: ResourceHandle) -> Vec<OperationId> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let operation = slot.operation.as_ref()?;
                (operation.resource == Some(resource)).then(|| self.id_at(index, slot.generation))
            })
            .collect()
    }

    /// Resolves a caller-supplied id to a slot index, validating it fully
    /// against this registry before any status/driver/cleanup/free-list
    /// mutation is allowed to proceed.
    fn location(&self, id: OperationId) -> OperationResult<usize> {
        if id.registry_tag() != self.tag {
            return Err(operation_wrong_registry(id));
        }
        let slot_index = id.slot_index();
        if slot_index >= self.slots.len() {
            return Err(operation_not_found(id));
        }
        let slot = &self.slots[slot_index];
        if id.generation() > slot.generation {
            // A future generation means the occupant does not exist yet.
            return Err(operation_not_found(id));
        }
        if id.generation() < slot.generation || slot.operation.is_none() {
            // Older generation or vacant (released) slot: the operation moved on.
            return Err(operation_stale(id));
        }
        Ok(slot_index)
    }

    fn operation(&self, id: OperationId) -> OperationResult<&Operation> {
        let slot = self.location(id)?;
        self.slots[slot]
            .operation
            .as_ref()
            .ok_or_else(|| operation_stale(id))
    }

    /// Reconstructs the packed id for an occupied slot at its current
    /// generation.
    fn id_at(&self, slot_index: usize, generation: u64) -> OperationId {
        encode(self.tag, slot_index, generation).expect("occupied slot encodes a registry id")
    }

    /// Acquires a reusable slot for a new operation: pops an index from the
    /// free list, or grows the arena by one new slot up to `MAX_SLOT_IDENTITY`.
    fn acquire_slot(&mut self) -> OperationResult<usize> {
        if let Some(index) = self.free.pop() {
            return Ok(index);
        }
        if self.slots.len() >= MAX_SLOT_IDENTITY as usize {
            return Err(OperationError::new(
                OperationErrorCode::OperationIdExhausted,
                "vm::operation",
                "operation slot identity space exhausted",
            ));
        }
        self.slots.push(OperationSlot {
            generation: 1,
            operation: None,
        });
        Ok(self.slots.len() - 1)
    }

    /// Releases an occupied slot: drops the occupant, increments the
    /// generation, and recycles the slot for reuse — unless the generation is
    /// at `MAX_GENERATION`, in which case the slot retires permanently.
    fn release_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.operation = None;
        if slot.generation < MAX_GENERATION {
            slot.generation += 1;
            self.free.push(index);
        }
    }
}

impl Drop for OperationRegistry {
    fn drop(&mut self) {
        // Best-effort teardown: cancel pending operations so the owning
        // drivers can release resources. The summary is intentionally ignored;
        // counting failures is irrelevant while the registry is being dropped.
        let _ = self.cancel_all(OperationCancelReason::VmReset);
    }
}

/// Aggregate result of cancelling a batch of operations.
///
/// Each attempted *pending* operation counts toward `matched`; only an
/// operation that actually reaches `Cancelled` counts toward `cancelled`;
/// a driver or cleanup failure counts toward `failed` with the first error
/// stored. A failure never increases `cancelled`, so there is no false
/// success in a batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationCancelSummary {
    matched: usize,
    cancelled: usize,
    failed: usize,
    first_error: Option<OperationError>,
}

impl OperationCancelSummary {
    /// Number of pending operations the batch attempted to cancel.
    pub fn matched(&self) -> usize {
        self.matched
    }

    /// Number of operations that successfully reached `Cancelled`.
    pub fn cancelled(&self) -> usize {
        self.cancelled
    }

    /// Number of operations where cancellation (driver) or cleanup failed.
    pub fn failed(&self) -> usize {
        self.failed
    }

    /// The first driver or cleanup error encountered, if any.
    pub fn first_error(&self) -> Option<&OperationError> {
        self.first_error.as_ref()
    }

    /// Records the outcome of one attempted cancellation.
    fn record(&mut self, result: OperationResult<bool>) {
        self.matched += 1;
        match result {
            Ok(true) => self.cancelled += 1,
            Ok(false) => {
                // An attempted pending operation did not transition; it is
                // neither cancelled nor counted as a driver/cleanup failure.
            }
            Err(error) => {
                self.failed += 1;
                if self.first_error.is_none() {
                    self.first_error = Some(error);
                }
            }
        }
    }
}

fn operation_not_found(id: OperationId) -> OperationError {
    OperationError::new(
        OperationErrorCode::OperationNotFound,
        "vm::operation",
        format!("operation {} is not registered", id.raw()),
    )
    .with_value(id.raw())
}

fn operation_wrong_registry(id: OperationId) -> OperationError {
    OperationError::new(
        OperationErrorCode::OperationWrongRegistry,
        "vm::operation",
        format!("operation {} belongs to a different registry", id.raw()),
    )
    .with_value(id.raw())
}

fn operation_stale(id: OperationId) -> OperationError {
    OperationError::new(
        OperationErrorCode::OperationStale,
        "vm::operation",
        format!("operation {} refers to a stale slot generation", id.raw()),
    )
    .with_value(id.raw())
}

fn pending_outcome(id: OperationId) -> OperationError {
    OperationError::new(
        OperationErrorCode::OperationPending,
        "vm::operation",
        format!(
            "operation {} is still pending and has no terminal outcome",
            id.raw()
        ),
    )
    .with_value(id.raw())
}

/// Wraps a driver cancel failure into the `OperationDriverFailed` category so
/// a failed driver action never produces a false success or a false
/// `Cancelled` state.
fn driver_failure(error: OperationError) -> OperationError {
    OperationError::new(
        OperationErrorCode::OperationDriverFailed,
        "vm::operation",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use super::{OperationRegistry, OperationStatus};
    use crate::vm::operation::driver::{
        HostOperation, OperationCleanup, OperationOutcome, OperationSpec,
    };
    use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
    use crate::vm::operation::id::{MAX_REGISTRY_TAG, encode};
    use crate::vm::operation::reason::OperationCancelReason;
    use crate::vm::resource::ResourceHandle;

    #[test]
    fn default_capacity_registry_reports_tag_exhaustion_without_panicking() {
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(MAX_REGISTRY_TAG + 1);
        let _source =
            crate::vm::operation::id::test_seam::ScopedRegistryTagSource::install(&COUNTER);

        let error = match OperationRegistry::new() {
            Ok(_) => panic!("tag exhaustion must be fallible"),
            Err(error) => error,
        };
        assert_eq!(
            error.code(),
            OperationErrorCode::OperationRegistryTagExhausted
        );
        assert_eq!(error.limit(), Some(MAX_REGISTRY_TAG));
        assert_eq!(
            COUNTER.load(Ordering::SeqCst),
            MAX_REGISTRY_TAG + 1,
            "failed construction must not advance the exhausted source"
        );
    }

    struct TestWake(Arc<AtomicUsize>);
    impl std::task::Wake for TestWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn waker() -> Waker {
        Waker::from(Arc::new(TestWake(Arc::new(AtomicUsize::new(0)))))
    }
    fn cx() -> Context<'static> {
        // Leak one no-op waker so the context is valid for 'static. Tests
        // intentionally leak the waker for simplicity.
        let waker: &'static Waker = Box::leak(Box::new(waker()));
        Context::from_waker(waker)
    }

    /// Encodes a syntactically valid resource handle with the given slot.
    /// (arena=1, gen=1, slot=`slot`).
    fn handle_for_slot(slot: u64) -> ResourceHandle {
        ResourceHandle::encode(1, slot as usize, 1).expect("encoded handle should be valid")
    }

    /// Eagerly-completing fake driver that records every cancellation reason
    /// it receives and can be configured to fail on poll.
    struct RecordingDriver {
        cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
        fail_on_poll: Option<OperationError>,
    }

    impl RecordingDriver {
        fn completed() -> Self {
            Self {
                cancels: Arc::new(Mutex::new(Vec::new())),
                fail_on_poll: None,
            }
        }
        fn failed(error: OperationError) -> Self {
            Self {
                cancels: Arc::new(Mutex::new(Vec::new())),
                fail_on_poll: Some(error),
            }
        }
    }

    impl HostOperation for RecordingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            match &self.fail_on_poll {
                Some(error) => Poll::Ready(Err(error.clone())),
                None => Poll::Ready(Ok(())),
            }
        }
        fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
            self.cancels.lock().unwrap().push(reason);
            Ok(())
        }
    }

    /// A pining driver that stays pending until released, recording every
    /// cancellation reason forwarded to it.
    struct PendingDriver {
        release: Arc<Mutex<bool>>,
        cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
    }
    impl PendingDriver {
        fn pending(cancels: Arc<Mutex<Vec<OperationCancelReason>>>) -> Self {
            Self {
                release: Arc::new(Mutex::new(false)),
                cancels,
            }
        }
    }
    impl HostOperation for PendingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            if *self.release.lock().unwrap() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
            self.cancels.lock().unwrap().push(reason);
            Ok(())
        }
    }

    /// A pending driver whose cancel action always fails.
    struct CancelFailDriver {
        error: OperationError,
        cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
    }
    impl CancelFailDriver {
        fn failing(error: OperationError) -> Self {
            Self {
                error,
                cancels: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }
    impl HostOperation for CancelFailDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            Poll::Pending
        }
        fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
            self.cancels.lock().unwrap().push(reason);
            Err(self.error.clone())
        }
    }

    /// A distinct, minimal driver type proving registry dispatch never
    /// depends on a host domain enum.
    struct AlternateDriver;
    impl HostOperation for AlternateDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            Poll::Ready(Ok(()))
        }
        fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
            Ok(())
        }
    }

    /// A cleanup hook that always fails.
    fn failing_cleanup(tag: &'static str) -> OperationCleanup {
        Box::new(move |_outcome: &OperationOutcome| {
            Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "test::cleanup",
                format!("{tag} cleanup failed"),
            ))
        })
    }

    #[test]
    fn two_different_driver_types_coexist_without_domain_dispatch() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let from_recorder = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("recorder driver should start");
        let from_alternate = registry
            .start(OperationSpec::new(AlternateDriver))
            .expect("alternate driver should start");
        // Both live in one registry with no domain-specific enum or poller
        // table.
        assert_eq!(registry.active_count(), 2);
        assert_ne!(from_recorder, from_alternate);
        assert!(matches!(
            registry.poll(from_recorder, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        assert!(matches!(
            registry.poll(from_alternate, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn expired_deadline_with_ready_driver_completes_without_cancel() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            cancels: Arc::clone(&cancels),
            fail_on_poll: None,
        };
        let id = registry
            .start(
                OperationSpec::new(driver).with_deadline(Instant::now() - Duration::from_millis(1)),
            )
            .expect("operation should start");

        // A Ready driver result wins even though the deadline has elapsed.
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        // The deadline is not forwarded; the driver is never cancelled.
        assert!(
            cancels.lock().unwrap().is_empty(),
            "driver must not be cancelled"
        );
        // The terminal was consumed by poll; the id is stale now.
        assert_eq!(
            registry
                .status(id)
                .expect_err("consumed id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
    }

    #[test]
    fn expired_deadline_with_pending_driver_cancels_once_then_stale() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let id = registry
            .start(
                OperationSpec::new(PendingDriver::pending(Arc::clone(&cancels)))
                    .with_deadline(Instant::now() - Duration::from_millis(1)),
            )
            .expect("operation should start");

        // Only a pending driver falls through to the deadline, which cancels.
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Cancelled(
                OperationCancelReason::Deadline
            )))
        ));
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Deadline]
        );
        // The terminal was consumed by poll; the id is stale now.
        assert_eq!(
            registry
                .status(id)
                .expect_err("consumed id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
    }

    #[test]
    fn driver_ready_outcome_is_one_shot_and_old_id_stale() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        // Terminal delivered exactly once; every later access is stale.
        assert_eq!(
            registry.status(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
        assert_eq!(
            registry.take_outcome(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
    }

    #[test]
    fn driver_outcome_is_delivered_exactly_once_then_stale() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let error = OperationError::new(OperationErrorCode::OperationDriverFailed, "test", "boom");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::failed(error)))
            .expect("start");
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Failed(err)))
                if err.code() == OperationErrorCode::OperationDriverFailed
        ));
        // The outcome is delivered once; later access reports stale.
        assert_eq!(
            registry.status(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
        assert_eq!(
            registry.take_outcome(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
    }

    #[test]
    fn consuming_a_terminal_result_releases_capacity() {
        let mut registry = OperationRegistry::with_limit(1).expect("registry should be valid");
        let first = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("first should start");
        let exceeded = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect_err("second should exceed the single-operation ceiling");
        assert_eq!(exceeded.code(), OperationErrorCode::OperationLimitExceeded);

        // Driving the first to terminal releases capacity for a new op.
        assert!(matches!(
            registry.poll(first, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("capacity should be released once terminal");
    }

    #[test]
    fn complete_then_take_releases_slot_for_reuse_with_higher_generation() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let first = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("first should start");
        let first_slot = first.slot_index();
        let first_gen = first.generation();

        assert!(registry.complete(first).expect("complete"));
        assert!(matches!(
            registry.take_outcome(first).expect("take"),
            OperationOutcome::Completed
        ));
        // The freed slot is reused under an incremented generation.
        let second = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("second should reuse the freed slot");
        assert_eq!(
            second.slot_index(),
            first_slot,
            "slot identity preserved on reuse"
        );
        assert!(
            second.generation() > first_gen,
            "generation increments on reuse"
        );
        assert_eq!(
            registry
                .status(first)
                .expect_err("old id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
    }

    #[test]
    fn take_outcome_on_pending_is_a_noop() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let id = registry
            .start(OperationSpec::new(PendingDriver::pending(cancels)))
            .expect("start");
        // Pending entries yield OperationPending without releasing the slot.
        assert_eq!(
            registry.take_outcome(id).expect_err("pending").code(),
            OperationErrorCode::OperationPending
        );
        assert!(matches!(
            registry.status(id).expect("still queryable"),
            OperationStatus::Pending
        ));
        // The operation can still be completed after the failed take.
        assert!(registry.complete(id).expect("complete"));
        assert!(matches!(
            registry.take_outcome(id).expect("take"),
            OperationOutcome::Completed
        ));
    }

    #[test]
    fn cleanup_failure_yields_failed_outcome_one_shot_and_once() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let runs = Arc::new(AtomicUsize::new(0));
        let cleanup: OperationCleanup = Box::new({
            let runs = Arc::clone(&runs);
            move |_outcome: &OperationOutcome| {
                runs.fetch_add(1, Ordering::SeqCst);
                Err(OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "test::cleanup",
                    "cleanup failed",
                ))
            }
        });
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_cleanup(cleanup))
            .expect("start");
        assert_eq!(
            registry.complete(id).expect_err("cleanup failure").code(),
            OperationErrorCode::OperationCleanupFailed
        );
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Failed(failed)
                if failed.code() == OperationErrorCode::OperationCleanupFailed
        ));
        // The Failed state is delivered once by take_outcome, then stale.
        assert!(matches!(
            registry.take_outcome(id).expect("take"),
            OperationOutcome::Failed(failed)
                if failed.code() == OperationErrorCode::OperationCleanupFailed
        ));
        assert_eq!(
            registry.status(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "cleanup runs exactly once");
    }

    #[test]
    fn driver_cancel_failure_sets_failed_runs_cleanup_with_failed_and_returns_err() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let runs = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let cleanup: OperationCleanup = Box::new({
            let runs = Arc::clone(&runs);
            let received = Arc::clone(&received);
            move |outcome: &OperationOutcome| {
                runs.fetch_add(1, Ordering::SeqCst);
                received.lock().unwrap().push(outcome.clone());
                Ok(())
            }
        });
        let driver_error = OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "test",
            "cancel boom",
        );
        let id = registry
            .start(
                OperationSpec::new(CancelFailDriver::failing(driver_error)).with_cleanup(cleanup),
            )
            .expect("start");

        let error = registry
            .cancel(id, OperationCancelReason::Requested)
            .expect_err("driver cancel fails");
        assert_eq!(error.code(), OperationErrorCode::OperationDriverFailed);
        // No false Cancelled; the terminal status is Failed(first).
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Failed(failed)
                if failed.code() == OperationErrorCode::OperationDriverFailed
        ));
        // Cleanup ran once and received the Failed outcome.
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "cleanup runs once on driver failure"
        );
        assert!(matches!(
            received.lock().unwrap()[..],
            [OperationOutcome::Failed(_)]
        ));
    }

    #[test]
    fn cancel_all_mixed_summary_counts_and_first_error_is_deterministic() {
        let mut registry = OperationRegistry::with_limit(8).expect("registry should be valid");
        // (1) succeeds in cancelling.
        let ok_id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("success op");
        // (2) driver cancel fails.
        let driver_error = OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "test",
            "cancel boom",
        );
        let driver_fail_id = registry
            .start(OperationSpec::new(CancelFailDriver::failing(driver_error)))
            .expect("driver-fail op");
        // (3) driver cancels but cleanup fails.
        let cleanup_fail_id = registry
            .start(
                OperationSpec::new(RecordingDriver::completed())
                    .with_cleanup(failing_cleanup("bulk")),
            )
            .expect("cleanup-fail op");

        let summary = registry.cancel_all(OperationCancelReason::VmReset);
        assert_eq!(summary.matched(), 3);
        assert_eq!(summary.cancelled(), 1);
        assert_eq!(summary.failed(), 2);
        // Deterministic (ascending slot order): the driver failure is first.
        assert_eq!(
            summary.first_error().expect("first error").code(),
            OperationErrorCode::OperationDriverFailed
        );
        // cancel_all drains to quiescence: every previous id is stale now.
        for id in [ok_id, driver_fail_id, cleanup_fail_id] {
            assert_eq!(
                registry
                    .status(id)
                    .expect_err("drained id must be stale")
                    .code(),
                OperationErrorCode::OperationStale
            );
        }
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn cancel_all_forwards_the_same_reason_to_every_driver() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels_a = Arc::new(Mutex::new(Vec::new()));
        let cancels_b = Arc::new(Mutex::new(Vec::new()));
        let driver_a = RecordingDriver {
            cancels: Arc::clone(&cancels_a),
            fail_on_poll: None,
        };
        let driver_b = RecordingDriver {
            cancels: Arc::clone(&cancels_b),
            fail_on_poll: None,
        };
        let a = registry.start(OperationSpec::new(driver_a)).expect("a");
        let b = registry.start(OperationSpec::new(driver_b)).expect("b");

        let summary = registry.cancel_all(OperationCancelReason::VmReset);
        assert_eq!(summary.matched(), 2);
        assert_eq!(summary.cancelled(), 2);
        assert_eq!(
            cancels_a.lock().unwrap()[..],
            [OperationCancelReason::VmReset]
        );
        assert_eq!(
            cancels_b.lock().unwrap()[..],
            [OperationCancelReason::VmReset]
        );
        assert_eq!(registry.active_count(), 0);
        let _ = (a, b);
    }

    #[test]
    fn abort_cancels_driver_once_releases_slot_and_frees_capacity() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let id = registry
            .start(OperationSpec::new(PendingDriver::pending(Arc::clone(
                &cancels,
            ))))
            .expect("start");

        let id_slot = id.slot_index();
        let id_gen = id.generation();
        let cancelled = registry
            .abort(id, OperationCancelReason::Requested)
            .expect("abort of a pending op should succeed");
        assert!(cancelled, "a pending op must be cancelled by abort");

        // The driver was cancelled exactly once with the given reason.
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Requested]
        );
        // The slot is released immediately: the id is stale, no occupant is
        // left, active_count and len are both zero, and full capacity is back.
        assert_eq!(
            registry
                .status(id)
                .expect_err("aborted id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
        assert_eq!(
            registry
                .take_outcome(id)
                .expect_err("aborted id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());

        // The freed slot is reusable under an incremented generation.
        let replacement = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("aborted slot must be reusable immediately");
        assert_eq!(
            replacement.slot_index(),
            id_slot,
            "slot identity preserved on reuse after abort"
        );
        assert!(
            replacement.generation() > id_gen,
            "generation increments on reuse after abort"
        );
    }

    #[test]
    fn abort_releases_slot_even_when_driver_cancel_fails() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let driver_error = OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "test",
            "cancel boom",
        );
        let id = registry
            .start(OperationSpec::new(CancelFailDriver::failing(driver_error)))
            .expect("start");
        // The driver cancel failure is preserved as the first reason, but the
        // abort still consumes and releases the slot so capacity is restored.
        let error = registry
            .abort(id, OperationCancelReason::Requested)
            .expect_err("driver cancel failure must be surfaced");
        assert_eq!(error.code(), OperationErrorCode::OperationDriverFailed);
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        // Capacity is fully restored despite the failed cancel.
        registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("abort must free capacity even on a failed cancel");
        registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("second op must fit the two-slot ceiling");
    }

    #[test]
    fn abort_on_already_terminal_removes_without_cancelling_again() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let id = registry
            .start(OperationSpec::new(PendingDriver::pending(Arc::clone(
                &cancels,
            ))))
            .expect("start");
        assert!(registry.complete(id).expect("out-of-band complete"));
        assert_eq!(cancels.lock().unwrap().len(), 0, "driver not cancelled yet");

        // Aborting an already-terminal operation removes/discards the terminal
        // entry without invoking the driver a second time.
        let cancelled = registry
            .abort(id, OperationCancelReason::VmReset)
            .expect("abort of a terminal op must succeed");
        assert!(
            !cancelled,
            "already-terminal abort must not report cancelled"
        );
        assert_eq!(
            cancels.lock().unwrap().len(),
            0,
            "driver must not be re-cancelled"
        );
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn abort_on_stale_id_is_rejected_without_mutation() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        // Drive to terminal, then remove it so the id becomes stale.
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        registry
            .status(id)
            .expect_err("polling must consume the outcome and stale the id");

        // A stale id is rejected with the typed code and the registry stays
        // quiescent.
        let error = registry
            .abort(id, OperationCancelReason::Requested)
            .expect_err("stale abort must fail");
        assert_eq!(error.code(), OperationErrorCode::OperationStale);
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn cancel_for_resource_matches_exact_pending_operations() {
        let mut registry = OperationRegistry::with_limit(8).expect("registry should be valid");
        let resource_x = handle_for_slot(1);
        let resource_y = handle_for_slot(2);
        // A terminal resource-x operation must NOT be re-cancelled.
        let terminal = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_x))
            .expect("terminal x should start");
        assert!(registry.complete(terminal).expect("complete"));
        let a = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_x))
            .expect("a should start");
        let b = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_x))
            .expect("b should start");
        let c = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_y))
            .expect("c should start");

        let summary =
            registry.cancel_for_resource(resource_x, OperationCancelReason::ResourceClosed);
        assert_eq!(summary.matched(), 2);
        assert_eq!(summary.cancelled(), 2);
        // Matching pending (a, b) are cancelled and drained to stale.
        for id in [a, b] {
            assert_eq!(
                registry
                    .status(id)
                    .expect_err("drained id must be stale")
                    .code(),
                OperationErrorCode::OperationStale
            );
        }
        // The pre-existing terminal resource-x op was drained too, not
        // re-cancelled and not counted as a match.
        assert_eq!(
            registry
                .status(terminal)
                .expect_err("pre-termimal drained must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
        // Non-matching resource stays pending; terminal op is untouched.
        assert!(matches!(
            registry.status(c).expect("status"),
            OperationStatus::Pending
        ));
        assert_eq!(registry.active_count(), 1);
    }

    #[test]
    fn pending_driver_keeps_registry_pending_until_released() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(Mutex::new(false));
        let driver = PendingDriver {
            release: Arc::clone(&release),
            cancels,
        };
        let id = registry.start(OperationSpec::new(driver)).expect("start");
        assert!(matches!(registry.poll(id, &mut cx()), Poll::Pending));
        *release.lock().unwrap() = true;
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
    }

    #[test]
    fn explicit_cancel_reason_wins_over_a_later_deadline() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            cancels: Arc::clone(&cancels),
            fail_on_poll: None,
        };
        let id = registry
            .start(
                OperationSpec::new(driver).with_deadline(Instant::now() - Duration::from_millis(1)),
            )
            .expect("operation should start");
        // Explicit cancellation arrives before the (already elapsed) deadline
        // is observed, so the first recorded reason wins.
        assert!(
            registry
                .cancel(id, OperationCancelReason::Requested)
                .expect("explicit cancel")
        );
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Cancelled(
                OperationCancelReason::Requested
            )))
        ));
        // Deadline must not be forwarded to the driver.
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Requested]
        );
    }

    #[test]
    fn driver_cancel_is_idempotent_and_first_reason_is_kept() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            cancels: Arc::clone(&cancels),
            fail_on_poll: None,
        };
        let id = registry.start(OperationSpec::new(driver)).expect("start");

        assert!(
            registry
                .cancel(id, OperationCancelReason::Requested)
                .expect("first cancel transitions")
        );
        assert!(
            !registry
                .cancel(id, OperationCancelReason::Parent)
                .expect("second cancel is a no-op")
        );
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::Requested)
        ));
        // Driver notified exactly once, with the first reason only.
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Requested]
        );
    }

    #[test]
    fn slot_reused_after_terminal_remove_bumps_generation_and_old_stales() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let first = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("first should start");
        let first_slot = first.slot_index();
        let first_gen = first.generation();

        // Leave the first op terminal out-of-band, then discard it explicitly.
        assert!(registry.complete(first).expect("complete first"));
        assert!(
            registry
                .remove(first)
                .expect("remove returns status")
                .is_terminal()
        );
        assert_eq!(registry.active_count(), 0);

        // A new operation reuses the same slot with a higher generation.
        let second = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("second should start into the freed slot");
        assert_eq!(
            second.slot_index(),
            first_slot,
            "slot identity is preserved on reuse"
        );
        assert!(
            second.generation() > first_gen,
            "generation must increment on slot reuse"
        );
        // The old id is stale now.
        assert_eq!(
            registry
                .status(first)
                .expect_err("old id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
        // Removing the old id must not touch the new occupant.
        assert!(matches!(
            registry.status(second).expect("new occupant"),
            OperationStatus::Pending
        ));
    }

    #[test]
    fn two_registries_reject_foreign_id_without_driver_mutation() {
        let mut registry_a = OperationRegistry::with_limit(4).expect("a valid");
        let mut registry_b = OperationRegistry::with_limit(4).expect("b valid");
        let cancels_a = Arc::new(Mutex::new(Vec::new()));
        let driver_a = RecordingDriver {
            cancels: Arc::clone(&cancels_a),
            fail_on_poll: None,
        };
        let id_a = registry_a
            .start(OperationSpec::new(driver_a))
            .expect("a starts");
        // Place a live driver in B so we can assert it is never touched.
        let cancels_b = Arc::new(Mutex::new(Vec::new()));
        let driver_b = RecordingDriver {
            cancels: Arc::clone(&cancels_b),
            fail_on_poll: None,
        };
        registry_b
            .start(OperationSpec::new(driver_b))
            .expect("b starts");

        // A's id on B: wrong registry, before any status/driver/cleanup mutation.
        assert_eq!(
            registry_b
                .status(id_a)
                .expect_err("foreign id must be rejected")
                .code(),
            OperationErrorCode::OperationWrongRegistry
        );
        assert_eq!(
            registry_b
                .cancel(id_a, OperationCancelReason::Requested)
                .expect_err("foreign id must be rejected")
                .code(),
            OperationErrorCode::OperationWrongRegistry
        );
        assert_eq!(
            registry_b
                .remove(id_a)
                .expect_err("foreign id must be rejected")
                .code(),
            OperationErrorCode::OperationWrongRegistry
        );

        // No driver's cancel/status path was exercised on either registry.
        assert!(cancels_a.lock().unwrap().is_empty(), "A's driver untouched");
        assert!(cancels_b.lock().unwrap().is_empty(), "B's driver untouched");
        // A's operation is unaffected.
        assert!(matches!(
            registry_a.status(id_a).expect("a still queryable"),
            OperationStatus::Pending
        ));
    }

    #[test]
    fn forged_same_tag_future_slot_and_generation_are_rejected_without_mutation() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            cancels: Arc::clone(&cancels),
            fail_on_poll: None,
        };
        let tag = registry.start(OperationSpec::new(driver)).expect("start");

        // Future slot: valid tag but an index beyond the arena.
        let out_of_range = encode(tag.registry_tag(), 1_000_000, 1).expect("forged slot id");
        assert_eq!(
            registry
                .status(out_of_range)
                .expect_err("must be rejected")
                .code(),
            OperationErrorCode::OperationNotFound
        );
        assert_eq!(
            registry
                .cancel(out_of_range, OperationCancelReason::Requested)
                .expect_err("must be rejected")
                .code(),
            OperationErrorCode::OperationNotFound
        );

        // Future generation on an existing slot.
        let future = encode(tag.registry_tag(), tag.slot_index(), tag.generation() + 1)
            .expect("forged future-generation id");
        assert_eq!(
            registry
                .status(future)
                .expect_err("must be rejected")
                .code(),
            OperationErrorCode::OperationNotFound
        );

        // No driver was cancelled and the real operation is unaffected.
        assert!(cancels.lock().unwrap().is_empty());
        assert_eq!(registry.active_count(), 1);
        assert!(matches!(
            registry.status(tag).expect("real op queryable"),
            OperationStatus::Pending
        ));
    }

    #[test]
    fn seal_is_idempotent_and_start_rejected_while_existing_operation_queryable() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start before seal");

        registry.seal();
        assert!(registry.is_sealed());
        // Idempotent.
        registry.seal();
        assert!(registry.is_sealed());

        let sealed = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect_err("start must be rejected once sealed");
        assert_eq!(sealed.code(), OperationErrorCode::OperationRegistrySealed);

        // Existing operation stays fully queryable after sealing.
        assert!(registry.complete(id).expect("complete"));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Completed
        ));
        assert_eq!(registry.resource_of(id).expect("resource"), None);
        assert!(matches!(
            registry.take_outcome(id).expect("take"),
            OperationOutcome::Completed
        ));
    }

    #[test]
    fn cancel_all_drains_preexisting_terminal_and_pending_counts_only_pending() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        // Pre-existing terminal operations are present before the bulk cancel.
        let completed = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("completed should start");
        assert!(registry.complete(completed).expect("complete"));
        let runs = Arc::new(AtomicUsize::new(0));
        let cleanup: OperationCleanup = Box::new({
            let runs = Arc::clone(&runs);
            move |_outcome: &OperationOutcome| {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let failed = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_cleanup(cleanup))
            .expect("failed should start");
        let fail_error =
            OperationError::new(OperationErrorCode::OperationDriverFailed, "test", "boom");
        assert!(registry.fail(failed, fail_error).expect("fail"));
        // One pending operation is the only one counted.
        let pending = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("pending should start");

        let summary = registry.cancel_all(OperationCancelReason::Requested);
        // Only the pending attempt is matched/cancelled; terminal ops are not.
        assert_eq!(summary.matched(), 1);
        assert_eq!(summary.cancelled(), 1);
        assert_eq!(summary.failed(), 0);
        assert_eq!(summary.first_error(), None);
        // Every previous id — pending and pre-existing terminal — is stale.
        for id in [completed, failed, pending] {
            assert_eq!(
                registry
                    .status(id)
                    .expect_err("drained id must be stale")
                    .code(),
                OperationErrorCode::OperationStale
            );
        }
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        // Pre-existing failure construction ran its cleanup exactly once and
        // draining did not re-run it.
        assert_eq!(runs.load(Ordering::SeqCst), 1, "cleanup runs exactly once");
        // The drained capacity is reusable.
        let again = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("capacity is reusable after draining");
        assert!(matches!(
            registry.status(again).expect("again"),
            OperationStatus::Pending
        ));
    }

    #[test]
    fn cancel_for_resource_drains_matching_preterminal_keeps_nonmatching() {
        let mut registry = OperationRegistry::with_limit(8).expect("registry should be valid");
        let resource_x = handle_for_slot(1);
        let resource_y = handle_for_slot(2);
        // A pre-existing terminal operation for resource_x.
        let terminal_x = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_x))
            .expect("terminal x should start");
        assert!(registry.complete(terminal_x).expect("complete terminal x"));
        // A pending operation for resource_x.
        let pending_x = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_x))
            .expect("pending x should start");
        // A pending operation for a different resource (nonmatching).
        let other_y = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_y))
            .expect("y should start");

        let summary =
            registry.cancel_for_resource(resource_x, OperationCancelReason::ResourceClosed);
        // Only the matching pending attempt is counted.
        assert_eq!(summary.matched(), 1);
        assert_eq!(summary.cancelled(), 1);
        // Matching pending and matching pre-existing terminal are drained.
        for id in [terminal_x, pending_x] {
            assert_eq!(
                registry
                    .status(id)
                    .expect_err("matching id must be stale")
                    .code(),
                OperationErrorCode::OperationStale
            );
        }
        // Nonmatching entry is untouched.
        assert!(matches!(
            registry.status(other_y).expect("nonmatching"),
            OperationStatus::Pending
        ));
        assert_eq!(registry.active_count(), 1);
    }

    #[test]
    fn remove_on_pending_is_refused_without_any_mutation() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(Mutex::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        let cleanup: OperationCleanup = Box::new({
            let runs = Arc::clone(&runs);
            move |_outcome: &OperationOutcome| {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let driver = PendingDriver {
            release: Arc::clone(&release),
            cancels: Arc::clone(&cancels),
        };
        let id = registry
            .start(OperationSpec::new(driver).with_cleanup(cleanup))
            .expect("start");
        let generation = id.generation();

        // Removing a pending op is refused as OperationPending.
        assert_eq!(
            registry
                .remove(id)
                .expect_err("pending remove must be refused")
                .code(),
            OperationErrorCode::OperationPending
        );
        // Status, driver, cleanup, and slot generation are all unchanged.
        assert!(matches!(
            registry.status(id).expect("still queryable"),
            OperationStatus::Pending
        ));
        assert!(cancels.lock().unwrap().is_empty(), "driver not cancelled");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "cleanup did not run");
        assert_eq!(id.generation(), generation, "generation unchanged");
        assert_eq!(registry.active_count(), 1);
        assert!(!registry.is_empty());

        // A normal cancel then take still works on the same slot.
        assert!(
            registry
                .cancel(id, OperationCancelReason::Requested)
                .expect("cancel")
        );
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Requested]
        );
        assert!(matches!(
            registry.take_outcome(id).expect("take"),
            OperationOutcome::Cancelled(OperationCancelReason::Requested)
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "cleanup ran on cancel");
    }

    #[test]
    fn terminal_remove_discards_and_reuses_slot_within_generation_lifetime() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let first = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("first should start");
        let first_slot = first.slot_index();
        let first_gen = first.generation();

        // Reach terminal out-of-band, then explicitly discard.
        assert!(registry.complete(first).expect("complete"));
        assert!(
            registry
                .remove(first)
                .expect("terminal remove returns status")
                .is_terminal()
        );
        assert_eq!(registry.active_count(), 0);
        assert!(registry.is_empty());

        // The freed slot is reused under an incremented generation.
        let second = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("second should reuse the freed slot");
        assert_eq!(second.slot_index(), first_slot);
        assert!(second.generation() > first_gen);
        // The removed id is stale and does not alias the new occupant.
        assert_eq!(
            registry
                .status(first)
                .expect_err("removed id must be stale")
                .code(),
            OperationErrorCode::OperationStale
        );
        assert!(matches!(
            registry.status(second).expect("new occupant"),
            OperationStatus::Pending
        ));
    }
}
