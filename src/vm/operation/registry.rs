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

    /// Cancels every pending operation associated with `resource`.
    ///
    /// Snapshots only the pending operations matching that exact
    /// [`ResourceHandle`] and cancels each, returning an
    /// [`OperationCancelSummary`]: every attempted pending operation
    /// increments `matched`, only a successful `Cancelled` increments
    /// `cancelled`, and a driver/cleanup failure increments `failed` with the
    /// first error stored. Failures are isolated; every matching pending
    /// operation is still attempted.
    pub fn cancel_for_resource(
        &mut self,
        resource: ResourceHandle,
        reason: OperationCancelReason,
    ) -> OperationCancelSummary {
        let mut summary = OperationCancelSummary::default();
        for id in self.pending_ids_for_resource(resource) {
            summary.record(self.cancel(id, reason));
        }
        summary
    }

    /// Cancels all pending operations, returning an
    /// [`OperationCancelSummary`]: every attempted pending operation
    /// increments `matched`, only a successful `Cancelled` increments
    /// `cancelled`, and a driver/cleanup failure increments `failed` with the
    /// first error stored. Snapshot order over pending ids is deterministic
    /// (ascending slot index); all operations are still attempted.
    pub fn cancel_all(&mut self, reason: OperationCancelReason) -> OperationCancelSummary {
        let mut summary = OperationCancelSummary::default();
        for id in self.pending_ids() {
            summary.record(self.cancel(id, reason));
        }
        summary
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

    /// Removes a single operation regardless of status, returning its status and
    /// releasing its slot for reuse.
    pub fn remove(&mut self, id: OperationId) -> OperationResult<OperationStatus> {
        let index = self.location(id)?;
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

    fn pending_ids(&self) -> Vec<OperationId> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.operation
                    .as_ref()
                    .filter(|operation| !operation.status.is_terminal())
                    .map(|_| self.id_at(index, slot.generation))
            })
            .collect()
    }

    /// Pending ids associated with exactly `resource`, in ascending slot order.
    fn pending_ids_for_resource(&self, resource: ResourceHandle) -> Vec<OperationId> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let operation = slot.operation.as_ref()?;
                (operation.resource == Some(resource) && !operation.status.is_terminal())
                    .then(|| self.id_at(index, slot.generation))
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

impl Default for OperationRegistry {
    fn default() -> Self {
        Self::with_limit(DEFAULT_MAX_PENDING_OPERATIONS)
            .expect("default operation registry configuration should be valid")
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
    use crate::vm::operation::id::encode;
    use crate::vm::operation::reason::OperationCancelReason;
    use crate::vm::resource::ResourceHandle;

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
        // Every attempted pending operation is now terminal.
        assert!(matches!(
            registry.status(ok_id).expect("ok"),
            OperationStatus::Cancelled(_)
        ));
        assert!(matches!(
            registry.status(driver_fail_id).expect("driver fail"),
            OperationStatus::Failed(f) if f.code() == OperationErrorCode::OperationDriverFailed
        ));
        assert!(matches!(
            registry.status(cleanup_fail_id).expect("cleanup fail"),
            OperationStatus::Failed(f) if f.code() == OperationErrorCode::OperationCleanupFailed
        ));
        assert_eq!(registry.active_count(), 0);
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
        assert!(matches!(
            registry.status(a).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::ResourceClosed)
        ));
        assert!(matches!(
            registry.status(b).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::ResourceClosed)
        ));
        // Non-matching resource stays pending; terminal op is untouched.
        assert!(matches!(
            registry.status(c).expect("status"),
            OperationStatus::Pending
        ));
        assert!(matches!(
            registry.status(terminal).expect("status"),
            OperationStatus::Completed
        ));
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
}
