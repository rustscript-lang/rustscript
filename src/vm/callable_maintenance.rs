//! Opt-in maintenance of expired callable identity records only.
//!
//! The policy lives in the existing out-of-line typed module store. It never
//! owns guest values, capture cells, resources, or a second identity index.

use std::sync::Weak;

use super::host_state::ModuleStateStore;
use super::{CallableValue, Vm};

/// Fixed inspection budget applied after each new callable identity record.
///
/// Maintenance is disabled by default. At least three probes are necessary:
/// a removal can swap a fresh live tail into the cursor, requiring another
/// probe before advancing; additional work must pass other live records.
/// A configured budget is independent of the historical registration count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableIdentityMaintenanceConfig {
    probes_per_registration: usize,
}

impl CallableIdentityMaintenanceConfig {
    /// Returns `None` for budgets below three, without changing any VM policy.
    pub const fn new(probes_per_registration: usize) -> Option<Self> {
        if probes_per_registration < 3 {
            None
        } else {
            Some(Self {
                probes_per_registration,
            })
        }
    }

    /// Maximum Weak entries inspected per newly registered identity record.
    pub const fn probes_per_registration(self) -> usize {
        self.probes_per_registration
    }
}

/// O(1) snapshot of the configured policy and the existing identity vector.
///
/// Entries count records, not distinct callable pointers or resources. Owned
/// graph walks visit each pointer once; recovery clears identities before
/// readoption, while constructor and frame bindings mint fresh pointers.
/// Capacity is the Vec allocation's high-water mark; pruning releases expired
/// Weak references (freeing an Arc allocation when its last Weak is dropped)
/// but does not shrink or copy the vector. Counters saturate at `u64::MAX` and count
/// only work since the most recent configuration, including across resets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableIdentityMaintenanceStats {
    pub entries: usize,
    pub capacity: usize,
    pub probes_per_registration: usize,
    pub registrations: u64,
    pub probes: u64,
    pub retired: u64,
}

struct MaintenanceState {
    config: CallableIdentityMaintenanceConfig,
    cursor: usize,
    registrations: u64,
    probes: u64,
    retired: u64,
}

impl Vm {
    /// Enables/reconfigures maintenance, or disables it with `None`.
    ///
    /// This infallible metadata-only operation neither scans existing records
    /// nor changes callable authority. Enabling creates one private typed
    /// module-state entry, so `HostContext::is_module_state_empty()` can become
    /// false only after explicit opt-in. Scope resource/operation counts are
    /// unaffected. The policy survives reset/reuse and shutdown; its cursor is
    /// reset when identities are cleared. Replacing the entire VM uses the
    /// replacement VM's policy. Disabling removes the private entry.
    ///
    /// Each registration then probes at most the configured budget and drops
    /// only expired Weak records. No maintenance occurs while idle. For a
    /// fixed bound on simultaneously live identities, continued registrations
    /// bound retained history; enabling after a large backlog retires it
    /// incrementally. This is not resource or capture-cell collection.
    pub fn set_callable_identity_maintenance(
        &mut self,
        config: Option<CallableIdentityMaintenanceConfig>,
    ) {
        if let Some(config) = config {
            self.host.set_module_state(MaintenanceState {
                config,
                cursor: 0,
                registrations: 0,
                probes: 0,
                retired: 0,
            });
        } else {
            self.host.remove_module_state::<MaintenanceState>();
        }
    }

    /// Returns an O(1) policy snapshot, or `None` when maintenance is disabled.
    pub fn callable_identity_maintenance_stats(&self) -> Option<CallableIdentityMaintenanceStats> {
        let state = self.host.get_module_state::<MaintenanceState>()?;
        Some(CallableIdentityMaintenanceStats {
            entries: self.instance.owned_callables.len(),
            capacity: self.instance.owned_callables.capacity(),
            probes_per_registration: state.config.probes_per_registration,
            registrations: state.registrations,
            probes: state.probes,
            retired: state.retired,
        })
    }
}

pub(super) fn register(
    entries: &mut Vec<Weak<CallableValue>>,
    store: &mut ModuleStateStore,
    callable: Weak<CallableValue>,
) {
    entries.push(callable);
    after_registration(entries, store);
}

pub(super) fn after_registration(
    entries: &mut Vec<Weak<CallableValue>>,
    store: &mut ModuleStateStore,
) {
    let Some(state) = store.get_mut::<MaintenanceState>() else {
        return;
    };
    state.registrations = state.registrations.saturating_add(1);
    for _ in 0..state.config.probes_per_registration {
        if entries.is_empty() {
            state.cursor = 0;
            break;
        }
        if state.cursor >= entries.len() {
            state.cursor = 0;
        }
        state.probes = state.probes.saturating_add(1);
        // Failure to upgrade proves expiry, without retaining a guest root.
        // A successful temporary Arc is dropped before the next probe.
        if entries[state.cursor].upgrade().is_none() {
            entries.swap_remove(state.cursor);
            state.retired = state.retired.saturating_add(1);
            // Probe the swapped entry next, rather than skipping it.
        } else {
            state.cursor += 1;
        }
    }
}

pub(super) fn identities_cleared(store: &mut ModuleStateStore) {
    if let Some(state) = store.get_mut::<MaintenanceState>() {
        state.cursor = 0;
    }
}
