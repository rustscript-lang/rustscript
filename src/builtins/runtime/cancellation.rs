use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use super::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};
use super::resource::ResourceHandle;

pub const DEFAULT_MAX_PENDING_OPERATIONS: usize = 64;
const TERMINAL_BIT: u8 = 0x80;
const REASON_MASK: u8 = !TERMINAL_BIT;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(u64);

impl OperationId {
    pub fn from_raw(raw: u64) -> RuntimeResult<Self> {
        if raw == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorCode::OperationIdExhausted,
                "runtime::operation",
                "operation id zero is reserved",
            ));
        }
        Ok(Self(raw))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationOwner {
    HostBridge,
    Io,
    Http,
    #[cfg(feature = "sqlite")]
    Sqlite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CancellationReason {
    Requested = 1,
    Deadline = 2,
    VmReset = 3,
    Parent = 4,
    ResourceClosed = 5,
}

impl CancellationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Deadline => "deadline",
            Self::VmReset => "vm_reset",
            Self::Parent => "parent",
            Self::ResourceClosed => "resource_closed",
        }
    }

    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Requested),
            2 => Some(Self::Deadline),
            3 => Some(Self::VmReset),
            4 => Some(Self::Parent),
            5 => Some(Self::ResourceClosed),
            _ => None,
        }
    }
}

struct CancellationSignal {
    state: AtomicU8,
    deadline: Option<Instant>,
    children: Mutex<Vec<Weak<OperationCore>>>,
    propagation_error: Mutex<Option<RuntimeError>>,
}

impl CancellationSignal {
    fn mark_cancelled(&self, reason: CancellationReason) -> bool {
        self.state
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn cancel(&self, reason: CancellationReason) -> (bool, Option<RuntimeError>) {
        let transitioned = self.mark_cancelled(reason);
        let mut first_error = None;
        if transitioned {
            let children = self
                .children
                .lock()
                .expect("cancellation children lock should not be poisoned")
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            for child in children {
                if let Err(error) = child.cancel(reason)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }
        (transitioned, first_error)
    }

    fn store_propagation_error(&self, error: Option<RuntimeError>) {
        if let Some(error) = error {
            let mut stored = self
                .propagation_error
                .lock()
                .expect("cancellation propagation error lock should not be poisoned");
            if stored.is_none() {
                *stored = Some(error);
            }
        }
    }

    fn take_propagation_error(&self) -> Option<RuntimeError> {
        self.propagation_error
            .lock()
            .expect("cancellation propagation error lock should not be poisoned")
            .take()
    }

    fn reason(&self) -> Option<CancellationReason> {
        let state = self.state.load(Ordering::Acquire);
        if state == 0
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let (_, error) = self.cancel(CancellationReason::Deadline);
            self.store_propagation_error(error);
        }
        CancellationReason::from_raw(self.state.load(Ordering::Acquire) & REASON_MASK)
    }

    fn finish_success(&self) -> bool {
        self.state
            .compare_exchange(0, TERMINAL_BIT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish_cancelled(&self, requested: CancellationReason) -> CancellationReason {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let reason = CancellationReason::from_raw(state & REASON_MASK).unwrap_or(requested);
            if state & TERMINAL_BIT != 0 {
                return reason;
            }
            let terminal = TERMINAL_BIT | reason as u8;
            if self
                .state
                .compare_exchange(state, terminal, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return reason;
            }
        }
    }
}

#[derive(Clone)]
pub struct CancellationToken {
    id: OperationId,
    signal: Arc<CancellationSignal>,
}

impl CancellationToken {
    pub(crate) fn root() -> Self {
        Self {
            id: OperationId(u64::MAX),
            signal: Arc::new(CancellationSignal {
                state: AtomicU8::new(0),
                deadline: None,
                children: Mutex::new(Vec::new()),
                propagation_error: Mutex::new(None),
            }),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }

    pub fn reason(&self) -> Option<CancellationReason> {
        self.signal.reason()
    }

    pub fn cancel(&self, reason: CancellationReason) -> bool {
        let (transitioned, error) = self.signal.cancel(reason);
        self.signal.store_propagation_error(error);
        transitioned
    }

    pub(crate) fn take_propagation_error(&self) -> Option<RuntimeError> {
        self.signal.take_propagation_error()
    }

    pub(crate) fn mark_cancelled(&self, reason: CancellationReason) -> bool {
        self.signal.mark_cancelled(reason)
    }

    pub fn check(&self) -> RuntimeResult<()> {
        let Some(reason) = self.reason() else {
            return Ok(());
        };
        Err(RuntimeError::new(
            RuntimeErrorCode::OperationCancelled,
            "runtime::operation",
            format!(
                "operation {} was cancelled ({})",
                self.id.raw(),
                reason.as_str()
            ),
        )
        .with_value(self.id.raw()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    Pending,
    Completed,
    Cancelled(CancellationReason),
    Failed(RuntimeError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationEnd {
    Completed,
    Cancelled(CancellationReason),
    Failed(RuntimeError),
}

pub type OperationCleanup = Box<dyn FnOnce(OperationEnd) -> RuntimeResult<()> + Send + 'static>;

struct OperationInner {
    status: OperationStatus,
    cleanup: Option<OperationCleanup>,
    payload: Option<ResourceHandle>,
    resource: Option<ResourceHandle>,
}

struct RegistryInner {
    operations: Mutex<HashMap<OperationId, OperationState>>,
}

struct OperationCore {
    id: OperationId,
    owner: OperationOwner,
    token: CancellationToken,
    inner: Mutex<OperationInner>,
}

impl OperationCore {
    fn status(&self) -> OperationStatus {
        self.inner
            .lock()
            .expect("operation state lock should not be poisoned")
            .status
            .clone()
    }

    fn cancel(&self, reason: CancellationReason) -> RuntimeResult<bool> {
        let _ = self.token.reason();
        let (_, child_error) = self.token.signal.cancel(reason);
        let child_error = child_error.or_else(|| self.token.signal.take_propagation_error());
        let cleanup = {
            let mut inner = self
                .inner
                .lock()
                .expect("operation state lock should not be poisoned");
            if !matches!(inner.status, OperationStatus::Pending) {
                return Ok(false);
            }
            let reason = self.token.reason().unwrap_or(reason);
            let reason = self.token.signal.finish_cancelled(reason);
            inner.status = OperationStatus::Cancelled(reason);
            (inner.cleanup.take(), reason)
        };
        let cleanup_result = if let (Some(cleanup), reason) = cleanup {
            cleanup(OperationEnd::Cancelled(reason)).map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::OperationCleanupFailed,
                    "runtime::operation",
                    error.to_string(),
                )
                .with_value(self.id.raw())
            })
        } else {
            Ok(())
        };
        match (child_error, cleanup_result) {
            (Some(error), _) => Err(error),
            (None, Err(error)) => Err(error),
            (None, Ok(())) => Ok(true),
        }
    }

    fn complete(&self) -> RuntimeResult<bool> {
        if let Some(reason) = self.token.reason() {
            return self.cancel(reason);
        }
        self.finish(OperationStatus::Completed, OperationEnd::Completed)
    }

    fn fail(&self, error: RuntimeError) -> RuntimeResult<bool> {
        if let Some(reason) = self.token.reason() {
            return self.cancel(reason);
        }
        self.finish(
            OperationStatus::Failed(error.clone()),
            OperationEnd::Failed(error),
        )
    }

    fn finish(&self, status: OperationStatus, end: OperationEnd) -> RuntimeResult<bool> {
        let (cleanup, end) = {
            let mut inner = self
                .inner
                .lock()
                .expect("operation state lock should not be poisoned");
            if !matches!(inner.status, OperationStatus::Pending) {
                return Ok(false);
            }
            let end = if self.token.signal.finish_success() {
                inner.status = status;
                end
            } else {
                let reason = self
                    .token
                    .reason()
                    .expect("a failed success transition must carry cancellation");
                let reason = self.token.signal.finish_cancelled(reason);
                inner.status = OperationStatus::Cancelled(reason);
                OperationEnd::Cancelled(reason)
            };
            (inner.cleanup.take(), end)
        };
        let cleanup_result = if let Some(cleanup) = cleanup {
            cleanup(end).map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::OperationCleanupFailed,
                    "runtime::operation",
                    error.to_string(),
                )
                .with_value(self.id.raw())
            })
        } else {
            Ok(())
        };
        cleanup_result?;
        Ok(true)
    }

    fn attach_parent(self: &Arc<Self>, parent: &CancellationToken) -> RuntimeResult<()> {
        {
            let mut children = parent
                .signal
                .children
                .lock()
                .expect("cancellation children lock should not be poisoned");
            children.retain(|child| child.strong_count() > 0);
            children.push(Arc::downgrade(self));
        }
        if parent.is_cancelled() {
            self.cancel(parent.reason().unwrap_or(CancellationReason::Parent))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct OperationState {
    core: Arc<OperationCore>,
}

impl fmt::Debug for OperationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationState")
            .field("id", &self.id())
            .field("owner", &self.owner())
            .field("status", &self.status())
            .finish()
    }
}

impl OperationState {
    pub fn id(&self) -> OperationId {
        self.core.id
    }

    pub fn owner(&self) -> OperationOwner {
        self.core.owner
    }

    pub fn token(&self) -> CancellationToken {
        self.core.token.clone()
    }

    pub fn status(&self) -> OperationStatus {
        self.core.status()
    }

    pub fn set_payload(&self, payload: ResourceHandle) {
        self.core
            .inner
            .lock()
            .expect("operation state lock should not be poisoned")
            .payload = Some(payload);
    }

    #[cfg(feature = "sqlite")]
    pub(crate) fn set_cleanup(&self, cleanup: OperationCleanup) -> RuntimeResult<()> {
        let mut inner = self
            .core
            .inner
            .lock()
            .expect("operation state lock should not be poisoned");
        if !matches!(inner.status, OperationStatus::Pending) {
            return Err(RuntimeError::new(
                RuntimeErrorCode::OperationAlreadyFinished,
                "runtime::operation",
                "cannot attach cleanup to a terminal operation",
            )
            .with_value(self.id().raw()));
        }
        if inner.cleanup.is_some() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "runtime::operation",
                "operation cleanup is already configured",
            )
            .with_value(self.id().raw()));
        }
        inner.cleanup = Some(cleanup);
        Ok(())
    }

    pub fn payload(&self) -> Option<ResourceHandle> {
        self.core
            .inner
            .lock()
            .expect("operation state lock should not be poisoned")
            .payload
    }

    pub fn set_resource(&self, resource: ResourceHandle) {
        self.core
            .inner
            .lock()
            .expect("operation state lock should not be poisoned")
            .resource = Some(resource);
    }

    pub fn resource(&self) -> Option<ResourceHandle> {
        self.core
            .inner
            .lock()
            .expect("operation state lock should not be poisoned")
            .resource
    }

    pub fn cancel(&self, reason: CancellationReason) -> RuntimeResult<bool> {
        self.core.cancel(reason)
    }

    pub fn complete(&self) -> RuntimeResult<bool> {
        self.core.complete()
    }

    pub fn fail(&self, error: RuntimeError) -> RuntimeResult<bool> {
        self.core.fail(error)
    }

    fn build(
        id: OperationId,
        owner: OperationOwner,
        deadline: Option<Instant>,
        cleanup: Option<OperationCleanup>,
    ) -> Self {
        let token = CancellationToken {
            id,
            signal: Arc::new(CancellationSignal {
                state: AtomicU8::new(0),
                deadline,
                children: Mutex::new(Vec::new()),
                propagation_error: Mutex::new(None),
            }),
        };
        Self {
            core: Arc::new(OperationCore {
                id,
                owner,
                token,
                inner: Mutex::new(OperationInner {
                    status: OperationStatus::Pending,
                    cleanup,
                    payload: None,
                    resource: None,
                }),
            }),
        }
    }
}

pub struct OperationRegistry {
    max_pending: usize,
    next_id: u64,
    last_external_id: u64,
    inner: Arc<RegistryInner>,
}

impl OperationRegistry {
    pub fn with_limit(max_pending: usize) -> RuntimeResult<Self> {
        if max_pending == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "runtime::operation",
                "operation registry capacity must be positive",
            ));
        }
        Ok(Self {
            max_pending,
            next_id: 1,
            last_external_id: 0,
            inner: Arc::new(RegistryInner {
                operations: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn active_count(&self) -> usize {
        self.inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned")
            .values()
            .filter(|operation| !matches!(operation.status(), OperationStatus::Cancelled(_)))
            .count()
    }

    pub(crate) fn allocate_id(&mut self) -> RuntimeResult<OperationId> {
        let id = OperationId::from_raw(self.next_id)?;
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::OperationIdExhausted,
                "runtime::operation",
                "operation id space exhausted",
            )
        })?;
        Ok(id)
    }

    pub fn start_owned(
        &mut self,
        owner: OperationOwner,
        parent: Option<&CancellationToken>,
        deadline: Option<Instant>,
        cleanup: Option<OperationCleanup>,
    ) -> RuntimeResult<OperationState> {
        if self.active_count() >= self.max_pending {
            return Err(RuntimeError::new(
                RuntimeErrorCode::OperationLimitExceeded,
                "runtime::operation",
                "pending operation capacity has been reached",
            )
            .with_limit(self.max_pending));
        }
        let id = self.allocate_id()?;
        let operation = OperationState::build(id, owner, deadline, cleanup);
        if let Some(parent) = parent {
            operation.core.attach_parent(parent)?;
        }
        self.inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned")
            .insert(id, operation.clone());
        Ok(operation)
    }

    #[cfg(test)]
    pub fn register_external(
        &mut self,
        id: OperationId,
        owner: OperationOwner,
        parent: Option<&CancellationToken>,
        deadline: Option<Instant>,
        cleanup: Option<OperationCleanup>,
    ) -> RuntimeResult<OperationState> {
        self.retire_external_id(id)?;
        self.register_retired_external(id, owner, parent, deadline, cleanup)
    }

    pub(crate) fn retire_external_id(&mut self, id: OperationId) -> RuntimeResult<()> {
        if id.raw() <= self.last_external_id {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "runtime::operation",
                format!(
                    "external operation {} is not newer than the last external operation {}",
                    id.raw(),
                    self.last_external_id
                ),
            )
            .with_value(id.raw()));
        }
        let next_id = id.raw().checked_add(1).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::OperationIdExhausted,
                "runtime::operation",
                "operation id space exhausted",
            )
        })?;
        self.last_external_id = id.raw();
        self.next_id = self.next_id.max(next_id);
        Ok(())
    }

    pub(crate) fn register_retired_external(
        &mut self,
        id: OperationId,
        owner: OperationOwner,
        parent: Option<&CancellationToken>,
        deadline: Option<Instant>,
        cleanup: Option<OperationCleanup>,
    ) -> RuntimeResult<OperationState> {
        if id.raw() != self.last_external_id {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "runtime::operation",
                format!("external operation {} has not just been retired", id.raw()),
            )
            .with_value(id.raw()));
        }
        if self.active_count() >= self.max_pending {
            return Err(RuntimeError::new(
                RuntimeErrorCode::OperationLimitExceeded,
                "runtime::operation",
                "pending operation capacity has been reached",
            )
            .with_limit(self.max_pending));
        }
        let registered = self
            .inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned");
        if registered.contains_key(&id) {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "runtime::operation",
                format!("operation {} is already registered", id.raw()),
            )
            .with_value(id.raw()));
        }
        let operation = OperationState::build(id, owner, deadline, cleanup);
        drop(registered);
        if let Some(parent) = parent {
            operation.core.attach_parent(parent)?;
        }
        self.inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned")
            .insert(id, operation.clone());
        Ok(operation)
    }

    pub fn get(&self, id: OperationId) -> RuntimeResult<OperationState> {
        self.inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned")
            .get(&id)
            .cloned()
            .ok_or_else(|| operation_not_found(id))
    }

    pub fn operations_by_owner(&self, owner: OperationOwner) -> Vec<OperationState> {
        let operations = self.registered_operations();
        operations
            .into_iter()
            .filter(|operation| operation.owner() == owner)
            .collect()
    }

    pub fn operations_for_resource(&self, resource: ResourceHandle) -> Vec<OperationState> {
        let operations = self.registered_operations();
        operations
            .into_iter()
            .filter(|operation| operation.resource() == Some(resource))
            .collect()
    }

    pub fn cancel(&mut self, id: OperationId, reason: CancellationReason) -> RuntimeResult<bool> {
        self.take_operation(id)?.cancel(reason)
    }

    pub fn complete(&mut self, id: OperationId) -> RuntimeResult<bool> {
        self.take_operation(id)?.complete()
    }

    pub fn fail(&mut self, id: OperationId, error: RuntimeError) -> RuntimeResult<bool> {
        self.take_operation(id)?.fail(error)
    }

    pub fn cancel_all(&mut self, reason: CancellationReason) -> RuntimeResult<usize> {
        let operations = {
            let mut registered = self
                .inner
                .operations
                .lock()
                .expect("operation registry lock should not be poisoned");
            std::mem::take(&mut *registered)
        };
        let operations = operations.into_values().collect::<Vec<_>>();
        for operation in &operations {
            operation.token().mark_cancelled(reason);
        }
        let mut first_error = None;
        for operation in &operations {
            if let Err(error) = operation.cancel(reason) {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(operations
                .iter()
                .filter(|operation| matches!(operation.status(), OperationStatus::Cancelled(_)))
                .count()),
        }
    }

    fn registered_operations(&self) -> Vec<OperationState> {
        self.inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned")
            .values()
            .cloned()
            .collect()
    }

    fn take_operation(&mut self, id: OperationId) -> RuntimeResult<OperationState> {
        self.inner
            .operations
            .lock()
            .expect("operation registry lock should not be poisoned")
            .remove(&id)
            .ok_or_else(|| operation_not_found(id))
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
        let _ = self.cancel_all(CancellationReason::VmReset);
    }
}

fn operation_not_found(id: OperationId) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::OperationNotFound,
        "runtime::operation",
        format!("operation {} is not registered", id.raw()),
    )
    .with_value(id.raw())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::super::error::{RuntimeError, RuntimeErrorCode};
    use super::{
        CancellationReason, OperationId, OperationOwner, OperationRegistry, OperationStatus,
    };

    #[test]
    fn token_reports_the_first_cancellation_reason() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let operation = registry
            .start_owned(OperationOwner::Io, None, None, None)
            .expect("operation should start");
        let token = operation.token();
        assert!(token.cancel(CancellationReason::Deadline));
        assert!(!token.cancel(CancellationReason::Parent));
        assert_eq!(token.reason(), Some(CancellationReason::Deadline));
        assert_eq!(operation.status(), OperationStatus::Pending);
    }

    #[test]
    fn parent_cancellation_propagates_and_deadline_is_structured() {
        let mut registry = OperationRegistry::with_limit(4).expect("registry should be valid");
        let parent = registry
            .start_owned(OperationOwner::Http, None, None, None)
            .expect("parent should start");
        let child = registry
            .start_owned(OperationOwner::Http, Some(&parent.token()), None, None)
            .expect("child should start");
        assert!(
            parent
                .cancel(CancellationReason::Requested)
                .expect("parent cancellation should succeed")
        );
        assert_eq!(child.token().reason(), Some(CancellationReason::Requested));

        let deadline_parent = registry
            .start_owned(OperationOwner::Io, None, None, None)
            .expect("deadline parent should start");
        let expired = registry
            .start_owned(
                OperationOwner::Io,
                Some(&deadline_parent.token()),
                Some(Instant::now() - Duration::from_millis(1)),
                None,
            )
            .expect("deadline child should start");
        assert_eq!(expired.token().reason(), Some(CancellationReason::Deadline));
    }

    #[test]
    fn cancel_all_counts_children_cancelled_by_parent_propagation() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let parent = registry
            .start_owned(OperationOwner::Http, None, None, None)
            .expect("parent should start");
        let child = registry
            .start_owned(OperationOwner::Io, Some(&parent.token()), None, None)
            .expect("child should start");

        assert_eq!(
            registry
                .cancel_all(CancellationReason::VmReset)
                .expect("all operations should cancel"),
            2
        );
        assert_eq!(
            parent.status(),
            OperationStatus::Cancelled(CancellationReason::VmReset)
        );
        assert!(matches!(
            child.status(),
            OperationStatus::Cancelled(CancellationReason::Parent | CancellationReason::VmReset)
        ));
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn parent_cancellation_finishes_registered_children_and_releases_capacity() {
        let child_cleanup_count = Arc::new(AtomicUsize::new(0));
        let cleanup_count = Arc::clone(&child_cleanup_count);
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let parent = registry
            .start_owned(OperationOwner::Http, None, None, None)
            .expect("parent should start");
        let child = registry
            .start_owned(
                OperationOwner::Io,
                Some(&parent.token()),
                None,
                Some(Box::new(move |end| {
                    assert_eq!(
                        end,
                        super::OperationEnd::Cancelled(CancellationReason::Requested)
                    );
                    cleanup_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })),
            )
            .expect("child should start");

        assert!(
            parent
                .cancel(CancellationReason::Requested)
                .expect("parent should cancel")
        );

        assert_eq!(
            child.status(),
            OperationStatus::Cancelled(CancellationReason::Requested)
        );
        assert_eq!(child_cleanup_count.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_count(), 0);
        assert!(registry.get(child.id()).is_ok());
        registry
            .start_owned(OperationOwner::Io, None, None, None)
            .expect("parent cancellation should release registry capacity");
        assert!(
            !child
                .cancel(CancellationReason::Requested)
                .expect("child cancellation should remain idempotent")
        );
        assert_eq!(child_cleanup_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parent_cancellation_propagates_child_cleanup_failure() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let parent = registry
            .start_owned(OperationOwner::Http, None, None, None)
            .expect("parent should start");
        registry
            .start_owned(
                OperationOwner::Io,
                Some(&parent.token()),
                None,
                Some(Box::new(|_| {
                    Err(RuntimeError::new(
                        RuntimeErrorCode::OperationFailed,
                        "test::cleanup",
                        "child cleanup failed",
                    ))
                })),
            )
            .expect("child should start");

        let error = parent
            .cancel(CancellationReason::Requested)
            .expect_err("child cleanup failure should propagate");
        assert_eq!(error.code(), RuntimeErrorCode::OperationCleanupFailed);
    }

    #[test]
    fn completed_external_operation_ids_cannot_be_reused() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let id = OperationId::from_raw(7).expect("operation id should be valid");
        registry
            .register_external(id, OperationOwner::HostBridge, None, None, None)
            .expect("first external operation should register");
        registry
            .complete(id)
            .expect("external operation should complete");

        let error = registry
            .register_external(id, OperationOwner::HostBridge, None, None, None)
            .expect_err("completed external operation id must remain retired");
        assert_eq!(error.code(), RuntimeErrorCode::InvalidConfiguration);
    }

    #[test]
    fn rejected_external_operation_ids_are_retired() {
        let mut registry = OperationRegistry::with_limit(1).expect("registry should be valid");
        let active = registry
            .start_owned(OperationOwner::Io, None, None, None)
            .expect("capacity should be occupied");
        let id = OperationId::from_raw(7).expect("operation id should be valid");
        let error = registry
            .register_external(id, OperationOwner::HostBridge, None, None, None)
            .expect_err("external operation should exceed capacity");
        assert_eq!(error.code(), RuntimeErrorCode::OperationLimitExceeded);
        registry
            .complete(active.id())
            .expect("capacity should be released");

        let error = registry
            .register_external(id, OperationOwner::HostBridge, None, None, None)
            .expect_err("rejected external operation id must remain retired");
        assert_eq!(error.code(), RuntimeErrorCode::InvalidConfiguration);
    }

    #[test]
    fn attaching_children_prunes_completed_parent_links() {
        let mut registry = OperationRegistry::with_limit(2).expect("registry should be valid");
        let parent = registry
            .start_owned(OperationOwner::Http, None, None, None)
            .expect("parent should start");

        for _ in 0..32 {
            let child = registry
                .start_owned(OperationOwner::Io, Some(&parent.token()), None, None)
                .expect("child should start");
            registry
                .complete(child.id())
                .expect("child should complete");
        }

        let live_links = parent
            .token()
            .signal
            .children
            .lock()
            .expect("children lock")
            .len();
        assert!(live_links <= 1, "completed child links should be pruned");
    }
}
