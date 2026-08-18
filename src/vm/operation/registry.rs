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

    /// Returns the terminal result of an operation, or `OperationNotFound`
    /// if it does not exist. With a pending operation this returns an error;
    /// use `poll` to drive it to terminal first.
    pub fn outcome(&self, id: OperationId) -> OperationResult<OperationOutcome> {
        self.operation(id)?
            .status
            .terminal_outcome()
            .ok_or_else(|| pending_outcome(id))
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
    /// Returns `Poll::Pending` while running, or `Poll::Ready(outcome)` with
    /// the first terminal result. A deadline that has elapsed while the
    /// operation is pending cancels it with `OperationCancelReason::Deadline`
    /// (preserving an even earlier reason). Once terminal, later polls
    /// re-report the stored outcome.
    pub fn poll(
        &mut self,
        id: OperationId,
        cx: &mut Context<'_>,
    ) -> Poll<OperationResult<OperationOutcome>> {
        // Fast path: already terminal.
        if let Some(outcome) = self.operation(id)?.status.terminal_outcome() {
            return Poll::Ready(Ok(outcome));
        }

        // Deadline expiration is handled as a cancellation, not a driver
        // poll; the first reason wins even if a driver reached Ready.
        if let Some(deadline) = self.operation(id)?.deadline
            && Instant::now() >= deadline
        {
            self.cancel(id, OperationCancelReason::Deadline)?;
            return Poll::Ready(self.outcome(id));
        }

        let poll_result = {
            let entry = self.operation_mut(id)?;
            entry.driver.poll(cx)
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                // If a cancellation transitioned this operation between the
                // deadline check and this result, prefer the cancellation.
                if !self.operation(id)?.status.is_terminal() {
                    self.apply_outcome(id, OperationOutcome::Completed)?;
                }
                Poll::Ready(self.outcome(id))
            }
            Poll::Ready(Err(error)) => {
                if !self.operation(id)?.status.is_terminal() {
                    self.apply_outcome(id, OperationOutcome::Failed(error.clone()))?;
                }
                Poll::Ready(self.outcome(id))
            }
        }
    }

    /// Cancels one operation, forwarding the reason to its driver.
    ///
    /// Idempotent: the driver's [`HostOperation::cancel`] is invoked at most
    /// once, and the *first* recorded reason is preserved. Returns `Ok(true)`
    /// if this call performed the cancellation transition, otherwise `Ok(false)`.
    ///
    /// A driver cancel failure is propagated (wrapped as
    /// `OperationDriverFailed`) rather than producing a false success. The
    /// terminal cancelled status is already recorded before the driver is
    /// asked to stop; detailed cancel-failure terminal/stats policy is
    /// deferred to the lifecycle scope.
    pub fn cancel(
        &mut self,
        id: OperationId,
        reason: OperationCancelReason,
    ) -> OperationResult<bool> {
        let (transitioned, cleanup) = {
            let entry = self.operation_mut(id)?;
            if !matches!(entry.status, OperationStatus::Pending) {
                (false, None)
            } else {
                entry.status = OperationStatus::Cancelled(reason);
                entry.driver.cancel(reason).map_err(driver_failure)?;
                (true, entry.cleanup.take())
            }
        };
        if !transitioned {
            return Ok(false);
        }
        if let Some(cleanup) = cleanup {
            self.run_cleanup(cleanup, OperationOutcome::Cancelled(reason))?;
        }
        Ok(true)
    }

    /// Cancels every pending operation associated with `resource`.
    ///
    /// Returns how many operations were transitioned. Cleanup failures are
    /// isolated: every matching operation is still processed and the first
    /// error is returned.
    pub fn cancel_for_resource(
        &mut self,
        resource: ResourceHandle,
        reason: OperationCancelReason,
    ) -> OperationResult<usize> {
        let mut count = 0;
        let mut first_error = None;
        for id in self.operations_for_resource(resource) {
            match self.cancel(id, reason) {
                Ok(transitioned) if transitioned => count += 1,
                Ok(_) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }

    /// Cancels all pending operations, returning how many were transitioned.
    /// Cleanup failures are isolated and reported as the first error; every
    /// operation is still processed.
    pub fn cancel_all(&mut self, reason: OperationCancelReason) -> OperationResult<usize> {
        let mut count = 0;
        let mut first_error = None;
        for id in self.pending_ids() {
            match self.cancel(id, reason) {
                Ok(transitioned) if transitioned => count += 1,
                Ok(_) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }

    /// Marks an operation completed out-of-band (e.g. a host future resolved
    /// without a poll). Idempotent; returns `false` if already terminal.
    pub fn complete(&mut self, id: OperationId) -> OperationResult<bool> {
        self.finish(id, OperationOutcome::Completed)
    }

    /// Marks an operation failed out-of-band. Idempotent; returns `false` if
    /// already terminal.
    pub fn fail(&mut self, id: OperationId, error: OperationError) -> OperationResult<bool> {
        self.finish(id, OperationOutcome::Failed(error))
    }

    /// Removes terminal operation entries from the arena, freeing their slots for
    /// reuse. Returns how many entries were removed.
    pub fn prune_terminal(&mut self) -> usize {
        let mut removed = 0;
        let terminal_indices: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.operation
                    .as_ref()
                    .filter(|operation| operation.status.is_terminal())
                    .map(|_| index)
            })
            .collect();
        for index in terminal_indices {
            self.release_slot(index);
            removed += 1;
        }
        removed
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

    fn finish(&mut self, id: OperationId, outcome: OperationOutcome) -> OperationResult<bool> {
        if self.operation(id)?.status.is_terminal() {
            return Ok(false);
        }
        self.apply_outcome(id, outcome)?;
        Ok(true)
    }

    /// Applies a terminal outcome to a pending operation: records the status,
    /// runs the (once) cleanup and leaves the op terminal. No-op when already
    /// terminal.
    fn apply_outcome(&mut self, id: OperationId, outcome: OperationOutcome) -> OperationResult<()> {
        let cleanup = {
            let entry = self.operation_mut(id)?;
            if entry.status.is_terminal() {
                return Ok(());
            }
            entry.status = match &outcome {
                OperationOutcome::Completed => OperationStatus::Completed,
                OperationOutcome::Cancelled(reason) => OperationStatus::Cancelled(*reason),
                OperationOutcome::Failed(error) => OperationStatus::Failed(error.clone()),
            };
            entry.cleanup.take()
        };
        if let Some(cleanup) = cleanup {
            self.run_cleanup(cleanup, outcome)?;
        }
        Ok(())
    }

    fn run_cleanup(
        &mut self,
        cleanup: OperationCleanup,
        outcome: OperationOutcome,
    ) -> OperationResult<()> {
        cleanup(&outcome).map_err(|error| {
            OperationError::new(
                OperationErrorCode::OperationCleanupFailed,
                "vm::operation",
                error.to_string(),
            )
        })
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

    fn operation_mut(&mut self, id: OperationId) -> OperationResult<&mut Operation> {
        let slot = self.location(id)?;
        self.slots[slot]
            .operation
            .as_mut()
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
        // Best-effort teardown: notify the owning drivers so the host can
        // release resources. Isolation is irrelevant here because we are
        // dropping the registry.
        for id in self.pending_ids() {
            let _ = self.cancel(id, OperationCancelReason::VmReset);
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
/// a failed driver action never produces a false success. The status is
/// already recorded cancelled before this point; detailed cancel-failure
/// terminal/stats policy is deferred to the lifecycle scope.
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
    /// it receives and can be configured to fail.
    struct RecordingDriver {
        cancels: Arc<Mutex<Vec<OperationCancelReason>>>,
        fail: Option<OperationError>,
    }

    impl RecordingDriver {
        fn completed() -> Self {
            Self {
                cancels: Arc::new(Mutex::new(Vec::new())),
                fail: None,
            }
        }
        fn failed(error: OperationError) -> Self {
            Self {
                cancels: Arc::new(Mutex::new(Vec::new())),
                fail: Some(error),
            }
        }
        fn recorded(&self) -> Vec<OperationCancelReason> {
            self.cancels.lock().unwrap().clone()
        }
    }

    impl HostOperation for RecordingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            match &self.fail {
                Some(error) => Poll::Ready(Err(error.clone())),
                None => Poll::Ready(Ok(())),
            }
        }
        fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
            self.cancels.lock().unwrap().push(reason);
            Ok(())
        }
    }

    /// A pining driver that stays pending until told to complete.
    struct PendingDriver {
        release: Arc<Mutex<bool>>,
    }
    impl HostOperation for PendingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
            if *self.release.lock().unwrap() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
            Ok(())
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
    fn cancel_for_resource_only_cancels_matching_operations() {
        let mut registry = OperationRegistry::with_limit(8).expect("registry should be valid");
        let resource_a = handle_for_slot(1);
        let resource_b = handle_for_slot(2);
        let a = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_a))
            .expect("a should start");
        let b = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_a))
            .expect("b should start");
        let c = registry
            .start(OperationSpec::new(RecordingDriver::completed()).with_resource(resource_b))
            .expect("c should start");

        let cancelled = registry
            .cancel_for_resource(resource_a, OperationCancelReason::ResourceClosed)
            .expect("resource cancellation should succeed");
        assert_eq!(cancelled, 2);
        assert!(matches!(
            registry.status(a).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::ResourceClosed)
        ));
        assert!(matches!(
            registry.status(b).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::ResourceClosed)
        ));
        assert!(matches!(
            registry.status(c).expect("status"),
            OperationStatus::Pending
        ));
    }

    #[test]
    fn cancel_all_forwards_the_same_reason_to_every_driver() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels_a = Arc::new(Mutex::new(Vec::new()));
        let cancels_b = Arc::new(Mutex::new(Vec::new()));
        let driver_a = RecordingDriver {
            cancels: Arc::clone(&cancels_a),
            fail: None,
        };
        let driver_b = RecordingDriver {
            cancels: Arc::clone(&cancels_b),
            fail: None,
        };
        let a = registry.start(OperationSpec::new(driver_a)).expect("a");
        let b = registry.start(OperationSpec::new(driver_b)).expect("b");

        let cancelled = registry
            .cancel_all(OperationCancelReason::VmReset)
            .expect("all should cancel");
        assert_eq!(cancelled, 2);
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
    fn deadline_fires_cancel_with_deadline_and_notifies_owner() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            cancels: Arc::clone(&cancels),
            fail: None,
        };
        let id = registry
            .start(
                OperationSpec::new(driver).with_deadline(Instant::now() - Duration::from_millis(1)),
            )
            .expect("operation should start");

        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Cancelled(
                OperationCancelReason::Deadline
            )))
        ));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::Deadline)
        ));
        assert_eq!(
            cancels.lock().unwrap()[..],
            [OperationCancelReason::Deadline]
        );
    }

    #[test]
    fn first_cancellation_reason_wins_over_a_later_deadline() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let driver = RecordingDriver {
            cancels: Arc::clone(&cancels),
            fail: None,
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
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(OperationCancelReason::Requested)
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
            fail: None,
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
    fn out_of_band_complete_is_idempotent() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        assert!(registry.complete(id).expect("first complete transitions"));
        assert!(!registry.complete(id).expect("second complete is a no-op"));
        assert!(matches!(
            registry.outcome(id).expect("outcome"),
            OperationOutcome::Completed
        ));
    }

    #[test]
    fn driver_poll_failure_records_terminal_failed_status() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let error = OperationError::new(OperationErrorCode::OperationDriverFailed, "test", "boom");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::failed(error)))
            .expect("start");
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Failed(_)))
        ));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Failed(error) if error.code() == OperationErrorCode::OperationDriverFailed
        ));
    }

    #[test]
    fn cleanup_failure_is_isolated_across_cancel_all() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let failing_cleanup = |tag: &'static str| -> OperationCleanup {
            Box::new(move |_outcome: &OperationOutcome| {
                Err(OperationError::new(
                    OperationErrorCode::OperationDriverFailed,
                    "test::cleanup",
                    format!("{tag} cleanup failed"),
                ))
            })
        };
        let a = registry
            .start(
                OperationSpec::new(RecordingDriver::completed()).with_cleanup(failing_cleanup("a")),
            )
            .expect("a");
        let b = registry
            .start(
                OperationSpec::new(RecordingDriver::completed()).with_cleanup(failing_cleanup("b")),
            )
            .expect("b");
        let c = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("c");

        let error = registry
            .cancel_all(OperationCancelReason::VmReset)
            .expect_err("cleanup failure should surface as the first error");
        assert_eq!(error.code(), OperationErrorCode::OperationCleanupFailed);
        // Every operation still became terminal despite the cleanup failures.
        assert_eq!(registry.active_count(), 0);
        assert!(matches!(
            registry.status(a).expect("status"),
            OperationStatus::Cancelled(_)
        ));
        assert!(matches!(
            registry.status(b).expect("status"),
            OperationStatus::Cancelled(_)
        ));
        assert!(matches!(
            registry.status(c).expect("status"),
            OperationStatus::Cancelled(_)
        ));
    }

    #[test]
    fn pending_driver_keeps_registry_pending_until_released() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let release = Arc::new(Mutex::new(false));
        let id = registry
            .start(OperationSpec::new(PendingDriver {
                release: Arc::clone(&release),
            }))
            .expect("start");
        assert!(matches!(registry.poll(id, &mut cx()), Poll::Pending));
        *release.lock().unwrap() = true;
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
    }

    #[test]
    fn slot_reused_after_terminal_remove_bumps_generation_and_old_stales() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let first = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("first should start");
        let first_slot = first.slot_index();
        let first_gen = first.generation();

        // Drive the first op terminal, then remove it so its slot is freed.
        assert!(matches!(
            registry.poll(first, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
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
            fail: None,
        };
        let id_a = registry_a
            .start(OperationSpec::new(driver_a))
            .expect("a starts");
        // Place a live driver in B so we can assert it is never touched.
        let cancels_b = Arc::new(Mutex::new(Vec::new()));
        let driver_b = RecordingDriver {
            cancels: Arc::clone(&cancels_b),
            fail: None,
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
            fail: None,
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

        // Existing operation remains fully queryable after sealing.
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Completed
        ));
        assert_eq!(registry.resource_of(id).expect("resource"), None);
    }

    #[test]
    fn prune_terminal_releases_slot_for_reuse_and_stales_old_id() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let old = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("start");
        let old_slot = old.slot_index();
        let old_gen = old.generation();

        assert!(matches!(
            registry.poll(old, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Completed))
        ));
        // Pruning frees the terminal slot (advanced generation).
        let pruned = registry.prune_terminal();
        assert_eq!(pruned, 1);

        let new = registry
            .start(OperationSpec::new(RecordingDriver::completed()))
            .expect("reuses the pruned slot");
        assert_eq!(new.slot_index(), old_slot, "pruned slot reused");
        assert!(new.generation() > old_gen, "prune advances the generation");
        assert_eq!(
            registry
                .status(old)
                .expect_err("old id must be stale after prune")
                .code(),
            OperationErrorCode::OperationStale
        );
    }
}
