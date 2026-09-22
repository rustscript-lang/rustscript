//! Scoped bind-mode counters for integration tests.
//!
//! Enable the opt-in `bind-mode-test-hooks` Cargo feature on `pd-vm` to expose
//! [`BindModeTestScope`] and [`BindModeSnapshot`]. The feature is absent from
//! the default feature set, so ordinary release builds contain neither this
//! registry nor its instrumentation calls.
//!
//! A process has at most one active scope. `enter` waits for an earlier scope
//! to be dropped, which gives tests exclusive measurement ownership without a
//! global counter reset. A snapshot counts successful authoritative operations
//! whose instrumentation boundary was reached while that scope was active,
//! including operations performed by other threads. Dropping a scope discards
//! its private counters and wakes the next waiter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Counts for one bind-mode measurement scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BindModeSnapshot {
    /// Successful `bind_vm_cached` / `bind_vm_with_plan` dispatch installations.
    pub full_bind_installs: u64,
    /// Successful `Vm::new_bound` instantiations, including owned callbacks.
    pub bound_vm_instantiations: u64,
}

struct ScopeCounters {
    full_bind_installs: AtomicU64,
    bound_vm_instantiations: AtomicU64,
}

impl ScopeCounters {
    fn new() -> Self {
        Self {
            full_bind_installs: AtomicU64::new(0),
            bound_vm_instantiations: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> BindModeSnapshot {
        BindModeSnapshot {
            full_bind_installs: self.full_bind_installs.load(Ordering::Acquire),
            bound_vm_instantiations: self.bound_vm_instantiations.load(Ordering::Acquire),
        }
    }
}

struct ActiveScope {
    id: u64,
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
    pub fn enter() -> Self {
        let registry = global_registry();
        let mut state = registry.lock_state();
        while state.active.is_some() {
            state = registry
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let counters = Arc::new(ScopeCounters::new());
        state.active = Some(ActiveScope {
            id,
            counters: Arc::clone(&counters),
        });
        drop(state);
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

fn record(update: impl FnOnce(&ScopeCounters)) {
    let registry = global_registry();
    let state = registry.lock_state();
    if let Some(active) = state.active.as_ref() {
        update(&active.counters);
    }
}
