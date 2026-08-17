//! Operation registry: lifecycle, bounds, deadline, first cancellation
//! reason and terminal status tracking for host-agnostic operations.
//!
//! The registry drives object-safe [`HostOperation`] drivers directly. It
//! replaces the old `OperationOwner`/`OperationState`/`CancellationToken`
//! parent–child signal graph with a single map of operation entries, each
//! carrying its own driver, deadline, optional resource association and
//! terminal status. There is deliberately no standalone token tree and no
//! second cancellation framework: cancellation is recorded once (the first
//! reason wins) and forwarded to exactly the owning driver via
//! [`HostOperation::cancel`].

use std::collections::HashMap;
use std::task::{Context, Poll};
use std::time::Instant;

use crate::builtins::runtime::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};
use crate::builtins::runtime::resource::ResourceHandle;
use crate::vm::operation::driver::{
    HostOperation, OperationCleanup, OperationOutcome, OperationSpec,
};

/// Default ceiling for concurrently pending operations.
pub const DEFAULT_MAX_PENDING_OPERATIONS: usize = 64;

/// Monotonic, non-reusable operation identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(u64);

impl OperationId {
    /// Wraps a raw non-zero id.
    pub fn from_raw(raw: u64) -> RuntimeResult<Self> {
        if raw == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorCode::OperationIdExhausted,
                "vm::operation",
                "operation id zero is reserved",
            ));
        }
        Ok(Self(raw))
    }

    /// The raw numeric id (safe to pass across a dynamic host call where the
    /// id is the only capability token the script holds).
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Public, observable operation status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    /// Still running.
    Pending,
    /// Finished successfully.
    Completed,
    /// Cancelled; carries the first cancellation reason.
    Cancelled(crate::vm::CancellationReason),
    /// Failed with a runtime error.
    Failed(RuntimeError),
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

struct Operation {
    id: OperationId,
    driver: Box<dyn HostOperation>,
    deadline: Option<Instant>,
    resource: Option<ResourceHandle>,
    cleanup: Option<OperationCleanup>,
    status: OperationStatus,
}

/// Bounded registry of in-flight host operations.
///
/// Capacity limits the number of *pending* operations; an operation that has
/// reached a terminal state no longer counts against capacity, so consuming a
/// terminal result releases registry capacity for new operations.
///
/// This type is intentionally `!Sync` (no interior mutability for concurrent
/// access); it is owned and driven by a single thread.
pub struct OperationRegistry {
    max_pending: usize,
    next_id: u64,
    operations: HashMap<OperationId, Operation>,
}

impl OperationRegistry {
    /// Creates an empty registry with the given pending-operation ceiling.
    pub fn with_limit(max_pending: usize) -> RuntimeResult<Self> {
        if max_pending == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "vm::operation",
                "operation registry capacity must be positive",
            ));
        }
        Ok(Self {
            max_pending,
            next_id: 1,
            operations: HashMap::new(),
        })
    }

    /// The configured pending-operation ceiling.
    pub fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// Number of operations still pending.
    pub fn active_count(&self) -> usize {
        self.operations
            .values()
            .filter(|operation| !operation.status.is_terminal())
            .count()
    }

    /// Starts a new operation from a spec, enforcing the capacity ceiling and
    /// monotonic id allocation.
    pub fn start(&mut self, spec: OperationSpec) -> RuntimeResult<OperationId> {
        if self.active_count() >= self.max_pending {
            return Err(RuntimeError::new(
                RuntimeErrorCode::OperationLimitExceeded,
                "vm::operation",
                "pending operation capacity has been reached",
            )
            .with_limit(self.max_pending));
        }
        let raw = Self::allocate_id(&mut self.next_id)?;
        let id = OperationId::from_raw(raw).expect("allocated id is never zero");
        let operation = Operation {
            id,
            driver: spec.driver,
            deadline: spec.deadline,
            resource: spec.resource,
            cleanup: spec.cleanup,
            status: OperationStatus::Pending,
        };
        self.operations.insert(id, operation);
        Ok(id)
    }

    /// Observes the current status of an operation.
    pub fn status(&self, id: OperationId) -> RuntimeResult<OperationStatus> {
        Ok(self.operation(id)?.status.clone())
    }

    /// Returns the terminal result of an operation, or `OperationNotFound`
    /// if it does not exist. With a pending operation this returns an error;
    /// use `poll` to drive it to terminal first.
    pub fn outcome(&self, id: OperationId) -> RuntimeResult<OperationOutcome> {
        self.operation(id)?
            .status
            .terminal_outcome()
            .ok_or_else(|| pending_outcome(id))
    }

    /// The resource handle an operation is associated with, if any.
    pub fn resource_of(&self, id: OperationId) -> RuntimeResult<Option<ResourceHandle>> {
        Ok(self.operation(id)?.resource)
    }

    /// Ids of operations associated with the given resource handle.
    pub fn operations_for_resource(&self, resource: ResourceHandle) -> Vec<OperationId> {
        self.operations
            .values()
            .filter(|entry| entry.resource == Some(resource))
            .map(|entry| entry.id)
            .collect()
    }

    /// Drives the operation one step.
    ///
    /// Returns `Poll::Pending` while running, or `Poll::Ready(outcome)` with
    /// the first terminal result. A deadline that has elapsed while the
    /// operation is pending cancels it with `CancellationReason::Deadline`
    /// (preserving an even earlier reason). Once terminal, later polls
    /// re-report the stored outcome.
    pub fn poll(
        &mut self,
        id: OperationId,
        cx: &mut Context<'_>,
    ) -> Poll<RuntimeResult<OperationOutcome>> {
        // Fast path: already terminal.
        if let Some(outcome) = self.operation(id)?.status.terminal_outcome() {
            return Poll::Ready(Ok(outcome));
        }

        // Deadline expiration is handled as a cancellation, not a driver
        // poll; the first reason wins even if a driver reached Ready.
        if let Some(deadline) = self.operation(id)?.deadline
            && Instant::now() >= deadline
        {
            self.cancel(id, crate::vm::CancellationReason::Deadline)?;
            return Poll::Ready(self.outcome(id));
        }

        let poll_result = {
            let entry = self.operations.get_mut(&id).expect("operation exists");
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
    pub fn cancel(
        &mut self,
        id: OperationId,
        reason: crate::vm::CancellationReason,
    ) -> RuntimeResult<bool> {
        let (transitioned, cleanup) = {
            let entry = self.operation_mut(id)?;
            if !matches!(entry.status, OperationStatus::Pending) {
                (false, None)
            } else {
                entry.status = OperationStatus::Cancelled(reason);
                entry.driver.cancel(reason);
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
        reason: crate::vm::CancellationReason,
    ) -> RuntimeResult<usize> {
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
    pub fn cancel_all(&mut self, reason: crate::vm::CancellationReason) -> RuntimeResult<usize> {
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
    pub fn complete(&mut self, id: OperationId) -> RuntimeResult<bool> {
        self.finish(id, OperationOutcome::Completed)
    }

    /// Marks an operation failed out-of-band. Idempotent; returns `false` if
    /// already terminal.
    pub fn fail(&mut self, id: OperationId, error: RuntimeError) -> RuntimeResult<bool> {
        self.finish(id, OperationOutcome::Failed(error))
    }

    /// Removes terminal operation entries from the map, freeing bookkeeping.
    /// Returns how many entries were removed.
    pub fn prune_terminal(&mut self) -> usize {
        let before = self.operations.len();
        self.operations
            .retain(|_, entry| !entry.status.is_terminal());
        before - self.operations.len()
    }

    /// Removes a single operation regardless of status, returning its status.
    pub fn remove(&mut self, id: OperationId) -> RuntimeResult<OperationStatus> {
        self.operations
            .remove(&id)
            .map(|entry| entry.status)
            .ok_or_else(|| operation_not_found(id))
    }

    fn finish(&mut self, id: OperationId, outcome: OperationOutcome) -> RuntimeResult<bool> {
        if self.operation(id)?.status.is_terminal() {
            return Ok(false);
        }
        self.apply_outcome(id, outcome)?;
        Ok(true)
    }

    /// Applies a terminal outcome to a pending operation: records the status,
    /// runs the (once) cleanup and leaves the op terminal. No-op when already
    /// terminal.
    fn apply_outcome(&mut self, id: OperationId, outcome: OperationOutcome) -> RuntimeResult<()> {
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
    ) -> RuntimeResult<()> {
        cleanup(outcome).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::OperationCleanupFailed,
                "vm::operation",
                error.to_string(),
            )
        })
    }

    fn pending_ids(&self) -> Vec<OperationId> {
        self.operations
            .values()
            .filter(|entry| !entry.status.is_terminal())
            .map(|entry| entry.id)
            .collect()
    }

    fn operation(&self, id: OperationId) -> RuntimeResult<&Operation> {
        self.operations
            .get(&id)
            .ok_or_else(|| operation_not_found(id))
    }

    fn operation_mut(&mut self, id: OperationId) -> RuntimeResult<&mut Operation> {
        self.operations
            .get_mut(&id)
            .ok_or_else(|| operation_not_found(id))
    }

    fn allocate_id(next_id: &mut u64) -> RuntimeResult<u64> {
        let id = OperationId::from_raw(*next_id)?;
        let raw = id.raw();
        *next_id = raw.checked_add(1).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::OperationIdExhausted,
                "vm::operation",
                "operation id space exhausted",
            )
        })?;
        Ok(raw)
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
            let _ = self.cancel(id, crate::vm::CancellationReason::VmReset);
        }
    }
}

fn operation_not_found(id: OperationId) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::OperationNotFound,
        "vm::operation",
        format!("operation {} is not registered", id.raw()),
    )
    .with_value(id.raw())
}

fn pending_outcome(id: OperationId) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::OperationCancelled,
        "vm::operation",
        format!(
            "operation {} is still pending and has no terminal outcome",
            id.raw()
        ),
    )
    .with_value(id.raw())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use super::{OperationRegistry, OperationStatus};
    use crate::builtins::runtime::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};
    use crate::builtins::runtime::resource::ResourceHandle;
    use crate::vm::operation::driver::{
        HostOperation, OperationCleanup, OperationOutcome, OperationSpec,
    };
    use crate::vm::{CancellationReason, Value};

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
    /// (arena=1, gen=1, type=1, slot=`slot`).
    fn handle_for_slot(slot: u64) -> ResourceHandle {
        ResourceHandle::from_value(&crate::vm::Value::Int(
            ((1i64) << 43) | ((slot as i64) << 25) | (1i64 << 8) | 1,
        ))
        .expect("encoded handle should be valid")
    }

    /// Eagerly-completing fake driver that records every cancellation reason
    /// it receives and can be configured to fail.
    struct RecordingDriver {
        cancels: Arc<Mutex<Vec<CancellationReason>>>,
        fail: Option<RuntimeError>,
    }

    impl RecordingDriver {
        fn completed() -> Self {
            Self {
                cancels: Arc::new(Mutex::new(Vec::new())),
                fail: None,
            }
        }
        fn failed(error: RuntimeError) -> Self {
            Self {
                cancels: Arc::new(Mutex::new(Vec::new())),
                fail: Some(error),
            }
        }
        fn recorded(&self) -> Vec<CancellationReason> {
            self.cancels.lock().unwrap().clone()
        }
    }

    impl HostOperation for RecordingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<RuntimeResult<()>> {
            match &self.fail {
                Some(error) => Poll::Ready(Err(error.clone())),
                None => Poll::Ready(Ok(())),
            }
        }
        fn cancel(&mut self, reason: CancellationReason) {
            self.cancels.lock().unwrap().push(reason);
        }
    }

    /// A pining driver that stays pending until told to complete.
    struct PendingDriver {
        release: Arc<Mutex<bool>>,
    }
    impl HostOperation for PendingDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<RuntimeResult<()>> {
            if *self.release.lock().unwrap() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn cancel(&mut self, _reason: CancellationReason) {}
    }

    /// A distinct, minimal driver type proving registry dispatch never
    /// depends on a host domain enum.
    struct AlternateDriver;
    impl HostOperation for AlternateDriver {
        fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<RuntimeResult<()>> {
            Poll::Ready(Ok(()))
        }
        fn cancel(&mut self, _reason: CancellationReason) {}
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
            .cancel_for_resource(resource_a, CancellationReason::ResourceClosed)
            .expect("resource cancellation should succeed");
        assert_eq!(cancelled, 2);
        assert!(matches!(
            registry.status(a).expect("status"),
            OperationStatus::Cancelled(CancellationReason::ResourceClosed)
        ));
        assert!(matches!(
            registry.status(b).expect("status"),
            OperationStatus::Cancelled(CancellationReason::ResourceClosed)
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
            .cancel_all(CancellationReason::VmReset)
            .expect("all should cancel");
        assert_eq!(cancelled, 2);
        assert_eq!(cancels_a.lock().unwrap()[..], [CancellationReason::VmReset]);
        assert_eq!(cancels_b.lock().unwrap()[..], [CancellationReason::VmReset]);
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
                CancellationReason::Deadline
            )))
        ));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(CancellationReason::Deadline)
        ));
        assert_eq!(cancels.lock().unwrap()[..], [CancellationReason::Deadline]);
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
                .cancel(id, CancellationReason::Requested)
                .expect("explicit cancel")
        );
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Cancelled(
                CancellationReason::Requested
            )))
        ));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(CancellationReason::Requested)
        ));
        // Deadline must not be forwarded to the driver.
        assert_eq!(cancels.lock().unwrap()[..], [CancellationReason::Requested]);
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
                .cancel(id, CancellationReason::Requested)
                .expect("first cancel transitions")
        );
        assert!(
            !registry
                .cancel(id, CancellationReason::Parent)
                .expect("second cancel is a no-op")
        );
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Cancelled(CancellationReason::Requested)
        ));
        // Driver notified exactly once, with the first reason only.
        assert_eq!(cancels.lock().unwrap()[..], [CancellationReason::Requested]);
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
        assert_eq!(exceeded.code(), RuntimeErrorCode::OperationLimitExceeded);

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
        let error = RuntimeError::new(RuntimeErrorCode::OperationFailed, "test", "boom");
        let id = registry
            .start(OperationSpec::new(RecordingDriver::failed(error)))
            .expect("start");
        assert!(matches!(
            registry.poll(id, &mut cx()),
            Poll::Ready(Ok(OperationOutcome::Failed(_)))
        ));
        assert!(matches!(
            registry.status(id).expect("status"),
            OperationStatus::Failed(error) if error.code() == RuntimeErrorCode::OperationFailed
        ));
    }

    #[test]
    fn cleanup_failure_is_isolated_across_cancel_all() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let failing_cleanup = |tag: &'static str| -> OperationCleanup {
            Box::new(move |_outcome: OperationOutcome| {
                Err(RuntimeError::new(
                    RuntimeErrorCode::OperationFailed,
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
            .cancel_all(CancellationReason::VmReset)
            .expect_err("cleanup failure should surface as the first error");
        assert_eq!(error.code(), RuntimeErrorCode::OperationCleanupFailed);
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
}
