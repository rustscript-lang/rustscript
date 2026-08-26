//! Operation registry: slot lifecycle, bounds, deadline and first-reason
//! cancellation tracking for host-agnostic operations.
//!
//! The registry owns a bounded, reusable generational slot arena. Each
//! occupied slot owns an object-safe [`HostOperation`] driver plus an
//! optional deadline, cleanup and its own status. Packed
//! `tag`/`slot`/`generation` ids are fully validated against the live slot
//! descriptor before any mutation, so a foreign-tagged, stale or
//! out-of-range id is rejected rather than aliased to a newer occupant.
//!
//! Cancellation is first-reason-wins, recorded once, and forwarded only to
//! the owning concrete driver via [`HostOperation::cancel`]. There is no
//! host-domain dispatch, no owner/poller table, and no secondary
//! cancellation channel.

use std::task::{Context, Poll};
use std::time::Instant;

use super::driver::{HostOperation, OperationCleanup, OperationOutcome, OperationSpec};
use super::error::{OperationError, OperationErrorCode, OperationResult};
use super::id::{MAX_GENERATION, MAX_SLOT_IDENTITY, OperationId, allocate_registry_tag, encode};
use super::reason::OperationCancelReason;

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
    /// registry; drive it to terminal with `poll` first. A terminal operation
    /// whose driver still owns an underlying worker stays pending until the
    /// worker reports quiescence.
    pub fn take_outcome(&mut self, id: OperationId) -> OperationResult<OperationOutcome> {
        let slot = self.location(id)?;
        let operation = self.slots[slot]
            .operation
            .as_ref()
            .ok_or_else(|| operation_stale(id))?;
        if !operation.driver.is_quiescent() {
            return Err(pending_outcome(id));
        }
        let status = operation.status.clone();
        let outcome = status
            .terminal_outcome()
            .ok_or_else(|| pending_outcome(id))?;
        self.release_slot(slot);
        Ok(outcome)
    }

    /// Drives the operation one step.
    ///
    /// Polls the owning driver first; a `Ready` driver result wins even if a
    /// deadline has already elapsed. Only a pending driver result falls
    /// through to the deadline check, in which case an elapsed deadline
    /// cancels the operation with `OperationCancelReason::Deadline`.
    ///
    /// The terminal outcome is delivered exactly once: when this returns
    /// `Poll::Ready`, the operation's slot is released and the id becomes
    /// stale. A cancelled terminal whose driver still owns a worker remains
    /// pending until that worker reports quiescence.
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

        // An out-of-band terminal (complete/fail/cancel) is consumed one-shot,
        // but only after the driver's underlying work is quiescent.
        if self.slots[slot]
            .operation
            .as_ref()
            .is_some_and(|operation| operation.status.is_terminal())
        {
            return self.poll_terminal(slot, cx);
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
                let operation = self.slots[slot]
                    .operation
                    .as_mut()
                    .expect("cancelled deadline operation remains occupied");
                if !operation.driver.is_quiescent() {
                    operation.driver.register_quiescence_waker(cx);
                    return Poll::Pending;
                }
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
                self.poll_terminal(slot, cx)
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
                self.poll_terminal(slot, cx)
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
        self.cancel_with_wait(id, reason, false)
    }

    fn cancel_with_wait(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
        wait_for_worker: bool,
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
            if wait_for_worker {
                operation.driver.cancel_and_wait(reason)
            } else {
                operation.driver.cancel(reason)
            }
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
    /// sites that register an operation and then hit a fallible handoff
    /// before installing the pending-result adapter.
    ///
    /// - **Pending** — the driver is cancelled exactly once with `reason`
    ///   (first-reason-wins), the resulting terminal outcome is consumed and
    ///   the slot released, and `Ok(true)` is returned. If the driver's
    ///   `cancel` itself fails, that failure is recorded as the first
    ///   `Failed` status, the cleanup runs once, the slot is still released,
    ///   and the driver error is returned — the slot is never left occupied
    ///   regardless of the cancel outcome.
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
        let _slot = self.location(id)?;
        let cancel_result = self.cancel_with_wait(id, reason, true);
        // Whether the driver cancelled cleanly, the driver's cancel failed
        // (the entry is now terminal `Failed`), or the entry was already
        // terminal before this call, consuming the outcome releases the slot
        // and makes the id stale exactly once. Preserve the first transition
        // error, while still surfacing an outcome-consumption error when the
        // cancellation itself succeeded.
        let take_result = self.take_outcome(id);
        match (cancel_result, take_result) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(cancelled), Ok(_)) => Ok(cancelled),
        }
    }

    /// Cancels every pending operation and records the outcome in a
    /// [`OperationCancelSummary`]. This is intentionally *cancel-only*: it
    /// records the first cancellation reason on each still-pending driver
    /// (and marks a failing driver's cancellation `Failed`), but it does **not**
    /// release any slot. A cancellation-aware worker may keep its terminal slot
    /// until a later [`poll_quiescence`](Self::poll_quiescence) call drives the
    /// driver to a terminal, quiescent state — the scope close driver relies on
    /// that to avoid claiming quiescence while a detached worker still owns
    /// resources.
    pub fn cancel_all(&mut self, reason: OperationCancelReason) -> OperationCancelSummary {
        let mut summary = OperationCancelSummary::default();
        for id in self.occupied_ids() {
            let is_pending = self
                .location(id)
                .ok()
                .and_then(|slot| self.slots[slot].operation.as_ref())
                .is_some_and(|operation| matches!(operation.status, OperationStatus::Pending));
            if !is_pending {
                // A pre-existing terminal operation is not matched; it is
                // drained later by `poll_quiescence`.
                continue;
            }
            let result = self.cancel(id, reason);
            summary.record(result);
        }
        summary
    }

    /// Polls cancellation-owned workers without blocking the VM thread. A
    /// terminal operation is released only after its driver reports actual
    /// quiescence. The driver owns the completion signal and wakes the scope
    /// through `register_quiescence_waker` when the transition occurs.
    pub fn poll_quiescence(&mut self, cx: &mut Context<'_>) -> bool {
        for id in self.occupied_ids() {
            let Ok(slot) = self.location(id) else {
                continue;
            };
            let Some(operation) = self.slots[slot].operation.as_mut() else {
                continue;
            };
            if !operation.status.is_terminal() {
                continue;
            }
            if operation.driver.is_quiescent() {
                let _ = self.consume_terminal(slot);
            } else {
                operation.driver.register_quiescence_waker(cx);
            }
        }
        self.is_empty()
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
            .is_some_and(|operation| {
                operation.status.is_terminal() && operation.driver.is_quiescent()
            });
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

    fn poll_terminal(
        &mut self,
        slot: usize,
        cx: &mut Context<'_>,
    ) -> Poll<OperationResult<OperationOutcome>> {
        let quiescent = {
            let operation = self.slots[slot]
                .operation
                .as_mut()
                .expect("terminal slot remains occupied");
            if operation.driver.is_quiescent() {
                true
            } else {
                operation.driver.register_quiescence_waker(cx);
                false
            }
        };
        if quiescent {
            Poll::Ready(Ok(self.consume_terminal(slot)))
        } else {
            Poll::Pending
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
    use crate::vm::operation::driver::{HostOperation, OperationOutcome, OperationSpec};
    use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
    use crate::vm::operation::id::{MAX_REGISTRY_TAG, encode};
    use crate::vm::operation::reason::OperationCancelReason;

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

    fn test_waker() -> (Waker, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(TestWake(Arc::clone(&wakes))));
        (waker, wakes)
    }

    /// Driver that completes immediately.
    struct RecordingDriver {
        polls: Arc<AtomicUsize>,
        cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
        completes: bool,
    }

    impl RecordingDriver {
        fn completed() -> Self {
            Self {
                polls: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(Mutex::new(Vec::new())),
                completes: true,
            }
        }

        fn pending() -> Self {
            Self {
                polls: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(Mutex::new(Vec::new())),
                completes: false,
            }
        }
    }

    impl HostOperation for RecordingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.completes {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
            self.cancels.lock().unwrap().push(reason);
            Ok(())
        }

        fn is_quiescent(&self) -> bool {
            self.completes
        }
    }

    /// Driver that stays pending until a shared gate releases it, recording
    /// every cancellation reason.
    struct PendingDriver {
        release: Arc<Mutex<bool>>,
        cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
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

        fn cancel_and_wait(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
            self.cancel(reason)?;
            *self.release.lock().unwrap() = true;
            Ok(())
        }

        fn is_quiescent(&self) -> bool {
            *self.release.lock().unwrap()
        }
    }

    /// Driver whose cancel fails with a typed error.
    struct CancelFailDriver;

    impl HostOperation for CancelFailDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            Poll::Pending
        }

        fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
            Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "test",
                "driver refused to cancel",
            ))
        }

        fn is_quiescent(&self) -> bool {
            true
        }
    }

    #[test]
    fn start_assigns_distinct_ids_and_capacity_is_bounded() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let a = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("first start");
        let b = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("second start");
        assert_ne!(a, b, "ids must be distinct");

        let error = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect_err("capacity reached");
        assert_eq!(error.code(), OperationErrorCode::OperationLimitExceeded);
        assert_eq!(error.limit(), Some(2));
    }

    #[test]
    fn complete_then_take_releases_slot_for_reuse_with_higher_generation() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let first = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        assert!(registry.complete(first).expect("complete"));
        assert_eq!(
            registry.take_outcome(first).expect("outcome"),
            OperationOutcome::Completed
        );
        assert_eq!(
            registry.status(first).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );

        // The slot is reused under an incremented generation.
        let second = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("reuse");
        assert_ne!(first, second, "reuse must mint a fresh id");
        assert!(registry.complete(second).expect("complete second"));
        assert_eq!(
            registry.take_outcome(second).expect("second outcome"),
            OperationOutcome::Completed
        );
    }

    #[test]
    fn take_outcome_on_pending_is_a_noop() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::pending()))
            .expect("start");
        let error = registry
            .take_outcome(id)
            .expect_err("pending has no outcome");
        assert_eq!(error.code(), OperationErrorCode::OperationPending);
        assert_eq!(registry.active_count(), 1);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn poll_drives_pending_to_completed_and_releases_slot() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        assert_eq!(registry.active_count(), 1);

        let (waker, _) = test_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(
            registry.poll(id, &mut cx),
            Poll::Ready(Ok(OperationOutcome::Completed))
        );
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 0);
        assert_eq!(
            registry.status(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
    }

    #[test]
    fn cancel_is_typed_and_first_reason_wins() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let id = registry
            .start(OperationSpec::new(PendingDriver {
                release: Arc::new(Mutex::new(false)),
                cancels: Arc::clone(&cancels),
            }))
            .expect("start");

        assert!(
            registry
                .cancel(id, OperationCancelReason::Requested)
                .expect("first cancel")
        );
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Requested]
        );
        // Second cancel is a no-op and preserves the first reason.
        assert!(
            !registry
                .cancel(id, OperationCancelReason::Deadline)
                .expect("terminal cancel is a no-op")
        );
        assert_eq!(cancels.lock().unwrap().len(), 1);
        assert_eq!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::Requested)
        );
    }

    #[test]
    fn cancel_all_mixed_summary_counts_and_first_error_is_deterministic() {
        let mut registry = OperationRegistry::with_limit(8).expect("registry");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let _clean = registry
            .start(OperationSpec::new(PendingDriver {
                release: Arc::new(Mutex::new(false)),
                cancels: Arc::clone(&cancels),
            }))
            .expect("clean pending");
        let _failing = registry
            .start(OperationSpec::new(CancelFailDriver))
            .expect("failing cancel");
        let _terminal = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("terminal");
        registry.complete(_terminal).expect("complete terminal");

        let summary = registry.cancel_all(OperationCancelReason::VmReset);
        assert_eq!(summary.matched(), 2, "only pending ops are matched");
        assert_eq!(summary.cancelled(), 1, "one clean cancellation");
        assert_eq!(summary.failed(), 1, "one failing cancellation");
        let first = summary.first_error().expect("first error");
        assert_eq!(first.code(), OperationErrorCode::OperationDriverFailed);
        // Cancel-only: terminal slots stay occupied until quiescence drains
        // the drivers.
        assert_eq!(registry.len(), 3, "all slots remain occupied after cancel");

        // Quiescence drains the pre-existing terminal and the cancellation
        // failure (whose driver has no worker); the still-running worker keeps
        // its slot.
        let (waker, _) = test_waker();
        let mut cx = Context::from_waker(&waker);
        registry.poll_quiescence(&mut cx);
        assert_eq!(registry.len(), 1, "only the running worker remains");
    }

    #[test]
    fn cancel_all_forwards_the_same_reason_to_every_driver() {
        let mut registry = OperationRegistry::with_limit(8).expect("registry");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..3 {
            registry
                .start(OperationSpec::new(PendingDriver {
                    release: Arc::new(Mutex::new(false)),
                    cancels: Arc::clone(&cancels),
                }))
                .expect("start");
        }
        let summary = registry.cancel_all(OperationCancelReason::Deadline);
        assert_eq!(summary.cancelled(), 3);
        let recorded = cancels.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        assert!(
            recorded
                .iter()
                .all(|reason| *reason == OperationCancelReason::Deadline)
        );
    }

    #[test]
    fn abort_cancels_driver_once_releases_slot_and_frees_capacity() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let id = registry
            .start(OperationSpec::new(PendingDriver {
                release: Arc::new(Mutex::new(false)),
                cancels: Arc::clone(&cancels),
            }))
            .expect("start");
        assert!(
            registry
                .abort(id, OperationCancelReason::VmReset)
                .expect("abort")
        );
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::VmReset]
        );
        assert_eq!(registry.len(), 0);
        assert_eq!(
            registry.status(id).expect_err("stale").code(),
            OperationErrorCode::OperationStale
        );
        // Capacity restored.
        registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("capacity restored");
    }

    #[test]
    fn abort_releases_slot_even_when_driver_cancel_fails() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let id = registry
            .start(OperationSpec::new(CancelFailDriver))
            .expect("start");
        let error = registry
            .abort(id, OperationCancelReason::VmReset)
            .expect_err("driver cancel failure surfaces");
        assert_eq!(error.code(), OperationErrorCode::OperationDriverFailed);
        assert_eq!(registry.len(), 0, "slot is still released");
        // Capacity restored even though cancellation failed.
        registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("capacity restored");
    }

    #[test]
    fn abort_on_already_terminal_removes_without_cancelling_again() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        assert!(registry.complete(id).expect("complete"));
        assert!(
            !registry
                .abort(id, OperationCancelReason::VmReset)
                .expect("terminal abort returns false")
        );
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn abort_on_stale_id_is_rejected_without_mutation() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        let foreign = encode(MAX_REGISTRY_TAG, 0, 1).expect("foreign id");
        let error = registry
            .abort(foreign, OperationCancelReason::VmReset)
            .expect_err("foreign id rejected");
        assert_eq!(error.code(), OperationErrorCode::OperationWrongRegistry);
        // The real operation is untouched.
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.status(id).expect("status"),
            OperationStatus::Pending
        );
    }

    #[test]
    fn deadline_cancels_pending_operation_with_deadline_reason() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let release = Arc::new(Mutex::new(false));
        let id = registry
            .start(
                OperationSpec::new(PendingDriver {
                    release: Arc::clone(&release),
                    cancels: Arc::new(Mutex::new(Vec::new())),
                })
                .with_deadline(Instant::now() - Duration::from_millis(1)),
            )
            .expect("start with elapsed deadline");

        let (waker, _) = test_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(registry.poll(id, &mut cx), Poll::Pending));
        *release.lock().unwrap() = true;
        match registry.poll(id, &mut cx) {
            Poll::Ready(Ok(OperationOutcome::Cancelled(OperationCancelReason::Deadline))) => {}
            other => panic!("expected deadline cancellation, got {other:?}"),
        }
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn cleanup_runs_exactly_once_on_terminal_transition() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let cleanups = Arc::new(AtomicUsize::new(0));
        let cleanups_for_hook = Arc::clone(&cleanups);
        let id = registry
            .start(
                OperationSpec::new(RecordingDriver::pending()).with_cleanup(Box::new(move |_| {
                    cleanups_for_hook.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })),
            )
            .expect("start with cleanup");

        assert!(
            registry
                .cancel(id, OperationCancelReason::Requested)
                .expect("cancel")
        );
        assert_eq!(cleanups.load(Ordering::SeqCst), 1, "cleanup ran once");
        // A second terminal transition is suppressed.
        assert!(!registry.complete(id).expect("second terminal is a no-op"));
        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remove_rejects_pending_and_removes_terminal() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let pending = registry
            .start(OperationSpec::new(RecordingDriver::pending()))
            .expect("pending");
        let error = registry.remove(pending).expect_err("pending not removable");
        assert_eq!(error.code(), OperationErrorCode::OperationPending);

        let terminal = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("terminal");
        registry.complete(terminal).expect("complete");
        assert_eq!(
            registry.remove(terminal).expect("remove terminal"),
            OperationStatus::Completed
        );
        assert_eq!(registry.len(), 1, "pending slot remains");
    }

    #[test]
    fn sealed_registry_rejects_new_starts() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::pending()))
            .expect("start before seal");
        registry.seal();
        assert!(registry.is_sealed());
        let error = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect_err("sealed rejects start");
        assert_eq!(error.code(), OperationErrorCode::OperationRegistrySealed);
        // Existing operations remain queryable.
        assert_eq!(
            registry.status(id).expect("status"),
            OperationStatus::Pending
        );
    }
}
