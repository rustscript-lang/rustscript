//! Scoped bind-mode counters for integration tests.
//!
//! Enable the opt-in `bind-mode-test-hooks` Cargo feature on `pd-vm` to expose
//! [`BindModeTestScope`] and [`BindModeSnapshot`]. The feature is absent from
//! the default feature set, so ordinary release builds contain neither this
//! registry nor its instrumentation calls.
//!
//! A process has at most one active scope. `enter` waits for an earlier scope
//! owned by another thread to be dropped, which gives tests exclusive
//! measurement ownership without a global counter reset. Same-thread re-entry
//! panics immediately; [`BindModeTestScope::try_enter`] reports it as an
//! explicit error instead. A snapshot counts successful authoritative
//! operations whose instrumentation boundary was reached while that scope was
//! active, including operations performed by other threads. Dropping a scope
//! discards its private counters and wakes the next waiter.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

/// Counts for one bind-mode measurement scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BindModeSnapshot {
    /// Successful `bind_vm_cached` / `bind_vm_with_plan` dispatch installations.
    pub full_bind_installs: u64,
    /// Successful `Vm::new_bound` instantiations, including owned callbacks.
    pub bound_vm_instantiations: u64,
    /// Successful new `BoundHostProgram` artifacts from `bind_program_once`.
    /// A host-plan cache hit does not increment this field; each call that
    /// publishes a new bound artifact increments it once.
    pub bound_program_preparations: u64,
}

/// Failure returned by [`BindModeTestScope::try_enter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindModeScopeError {
    /// The calling thread already owns the active measurement scope.
    AlreadyActiveOnCurrentThread,
    /// Another thread owns the active measurement scope.
    AlreadyActiveOnAnotherThread,
}

impl fmt::Display for BindModeScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyActiveOnCurrentThread => write!(
                formatter,
                "bind-mode test scope is already active on the current thread"
            ),
            Self::AlreadyActiveOnAnotherThread => write!(
                formatter,
                "bind-mode test scope is already active on another thread"
            ),
        }
    }
}

impl std::error::Error for BindModeScopeError {}

struct ScopeCounters {
    full_bind_installs: AtomicU64,
    bound_vm_instantiations: AtomicU64,
    bound_program_preparations: AtomicU64,
}

impl ScopeCounters {
    fn new() -> Self {
        Self {
            full_bind_installs: AtomicU64::new(0),
            bound_vm_instantiations: AtomicU64::new(0),
            bound_program_preparations: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> BindModeSnapshot {
        BindModeSnapshot {
            full_bind_installs: self.full_bind_installs.load(Ordering::Acquire),
            bound_vm_instantiations: self.bound_vm_instantiations.load(Ordering::Acquire),
            bound_program_preparations: self.bound_program_preparations.load(Ordering::Acquire),
        }
    }
}

struct ActiveScope {
    id: u64,
    owner: ThreadId,
    counters: Arc<ScopeCounters>,
}

struct ScopeRegistry {
    next_id: u64,
    active: Option<ActiveScope>,
}

struct GlobalRegistry {
    state: Mutex<ScopeRegistry>,
    changed: Condvar,
}

impl GlobalRegistry {
    fn new() -> Self {
        Self {
            state: Mutex::new(ScopeRegistry {
                next_id: 1,
                active: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ScopeRegistry> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn global_registry() -> &'static GlobalRegistry {
    static REGISTRY: OnceLock<GlobalRegistry> = OnceLock::new();
    REGISTRY.get_or_init(GlobalRegistry::new)
}

/// Exclusive measurement scope for authoritative VM bind-mode events.
///
/// `enter` waits until an earlier scope is dropped. The scope is safe to move
/// between threads; event recording and snapshots use atomic counters under a
/// synchronized active-scope lease. There is deliberately no process-global
/// reset operation.
pub struct BindModeTestScope {
    id: u64,
    counters: Arc<ScopeCounters>,
}

impl BindModeTestScope {
    /// Starts an exclusive scope with zeroed private counters.
    ///
    /// Waiting is allowed only when another thread owns the active scope. A
    /// same-thread nested entry panics immediately; use [`Self::try_enter`]
    /// when the caller needs a non-panicking result.
    pub fn enter() -> Self {
        let owner = std::thread::current().id();
        let registry = global_registry();
        let mut state = registry.lock_state();
        while state.active.is_some() {
            if state
                .active
                .as_ref()
                .is_some_and(|active| active.owner == owner)
            {
                panic!("bind-mode test scope cannot be entered recursively on the same thread");
            }
            state = registry
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        Self::install(&mut state, owner)
    }

    /// Attempts an exclusive scope without waiting.
    pub fn try_enter() -> Result<Self, BindModeScopeError> {
        let owner = std::thread::current().id();
        let registry = global_registry();
        let mut state = registry.lock_state();
        if let Some(active) = state.active.as_ref() {
            return Err(if active.owner == owner {
                BindModeScopeError::AlreadyActiveOnCurrentThread
            } else {
                BindModeScopeError::AlreadyActiveOnAnotherThread
            });
        }
        Ok(Self::install(&mut state, owner))
    }

    fn install(state: &mut ScopeRegistry, owner: ThreadId) -> Self {
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let counters = Arc::new(ScopeCounters::new());
        state.active = Some(ActiveScope {
            id,
            owner,
            counters: Arc::clone(&counters),
        });
        Self { id, counters }
    }

    /// Returns the monotonic counts observed by this scope.
    pub fn snapshot(&self) -> BindModeSnapshot {
        let registry = global_registry();
        let _state = registry.lock_state();
        self.counters.snapshot()
    }
}

impl Drop for BindModeTestScope {
    fn drop(&mut self) {
        let registry = global_registry();
        let mut state = registry.lock_state();
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.id == self.id)
        {
            state.active = None;
            registry.changed.notify_one();
        }
    }
}

pub(crate) fn record_full_bind_installation() {
    record(|counters| {
        counters.full_bind_installs.fetch_add(1, Ordering::Release);
    });
}

pub(crate) fn record_bound_vm_instantiation() {
    record(|counters| {
        counters
            .bound_vm_instantiations
            .fetch_add(1, Ordering::Release);
    });
}

pub(crate) fn record_bound_program_preparation() {
    record(|counters| {
        counters
            .bound_program_preparations
            .fetch_add(1, Ordering::Release);
    });
}

fn record(update: impl FnOnce(&ScopeCounters)) {
    let registry = global_registry();
    let state = registry.lock_state();
    if let Some(active) = state.active.as_ref() {
        update(&active.counters);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn same_thread_reentry_is_nonblocking_and_explicit() {
        let scope = BindModeTestScope::enter();
        assert!(matches!(
            BindModeTestScope::try_enter(),
            Err(BindModeScopeError::AlreadyActiveOnCurrentThread)
        ));
        let panic = std::panic::catch_unwind(|| BindModeTestScope::enter());
        assert!(panic.is_err(), "nested enter must fail without waiting");
        drop(scope);

        let scope = BindModeTestScope::try_enter().expect("scope released after reentry test");
        drop(scope);
    }

    #[test]
    fn cross_thread_try_enter_reports_busy_and_waiting_enter_wakes() {
        let owner_scope = BindModeTestScope::enter();
        let (try_result_tx, try_result_rx) = mpsc::channel();
        let try_thread = std::thread::spawn(move || {
            try_result_tx
                .send(BindModeTestScope::try_enter())
                .expect("try result receiver");
        });
        assert!(matches!(
            try_result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("try result"),
            Err(BindModeScopeError::AlreadyActiveOnAnotherThread)
        ));
        try_thread.join().expect("try thread");

        let (ready_tx, ready_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let waiting_thread = std::thread::spawn(move || {
            ready_tx.send(()).expect("ready receiver");
            let scope = BindModeTestScope::enter();
            acquired_tx.send(()).expect("acquired receiver");
            drop(scope);
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiting thread started");
        assert!(
            acquired_rx.recv_timeout(Duration::from_millis(25)).is_err(),
            "the waiting thread must not enter before release"
        );
        drop(owner_scope);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiting thread wakes after release");
        waiting_thread.join().expect("waiting thread");
    }

    #[test]
    fn panic_unwind_releases_the_scope_for_the_next_thread() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let panic_thread = std::thread::spawn(move || {
            let _scope = BindModeTestScope::enter();
            ready_tx.send(()).expect("ready receiver");
            panic!("scope owner panic");
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("panic thread started");
        assert!(panic_thread.join().is_err());

        let scope = BindModeTestScope::try_enter().expect("panic unwind released scope");
        drop(scope);
    }

    #[test]
    fn poisoned_registry_lock_is_recovered() {
        let registry = global_registry();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.state.lock().expect("unpoisoned registry lock");
            panic!("poison registry lock");
        }));
        assert!(panic.is_err());

        let scope = BindModeTestScope::enter();
        drop(scope);
    }
}
