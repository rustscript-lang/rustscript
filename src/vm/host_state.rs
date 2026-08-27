//! Generic lower-layer module-state store.
//!
//! [`ModuleStateStore`] is the single persistent, typed per-VM module-state
//! store owned by [`HostRuntime`](super::host_runtime::HostRuntime) and
//! surfaced to host extensions through
//! [`HostContext`](super::host_context::HostContext). It lives in the generic
//! VM layer so persistent policy/configuration storage is one generic
//! primitive that does not depend on [`HostContext`], [`HostModule`],
//! builtins, or any adapter feature.
//!
//! Entries are uniquely owned `Box<dyn Any + Send>` values keyed by
//! [`TypeId`], with typed `set` / `get` / `get_mut` / `remove` operations.
//! The store is deliberately opaque to the VM and survives execution-scope
//! reset for the lifetime of the owning runtime/`Vm`.

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// The typed per-VM module-state store.
///
/// Persistent policy/configuration lives here and is exposed through the
/// host-context boundary. State is typed at compile time (keyed by
/// [`TypeId`]) and survives scope reset / scope recycling.
#[derive(Default)]
pub(crate) struct ModuleStateStore {
    entries: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl ModuleStateStore {
    /// Creates an empty module-state store.
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Registers a typed module-state value, replacing any earlier value of
    /// the same type.
    ///
    /// Returns `true` when a previously registered value of the same type was
    /// replaced, and `false` when this value was freshly registered.
    pub(crate) fn set<T: Any + Send + 'static>(&mut self, state: T) -> bool {
        self.entries
            .insert(TypeId::of::<T>(), Box::new(state))
            .is_some()
    }

    /// Borrows the registered typed module state, if any.
    pub(crate) fn get<T: Any + Send + 'static>(&self) -> Option<&T> {
        self.entries
            .get(&TypeId::of::<T>())
            .and_then(|state| state.downcast_ref::<T>())
    }

    /// Borrows the registered typed module state mutably, if any.
    pub(crate) fn get_mut<T: Any + Send + 'static>(&mut self) -> Option<&mut T> {
        self.entries
            .get_mut(&TypeId::of::<T>())
            .and_then(|state| state.downcast_mut::<T>())
    }

    /// Removes and returns the registered typed module state.
    ///
    /// Returns the uniquely owned value, removing its store entry. No
    /// uniqueness invariant (`Arc::get_mut` style) is required because each
    /// entry is owned exclusively by this store.
    pub(crate) fn remove<T: Any + Send + 'static>(&mut self) -> Option<T> {
        self.entries
            .remove(&TypeId::of::<T>())
            .and_then(|state| state.downcast::<T>().ok())
            .map(|state| *state)
    }

    /// Returns `true` when no module state is currently registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::ModuleStateStore;

    #[derive(Debug, PartialEq)]
    struct DemoState {
        value: u64,
    }

    #[test]
    fn set_get_and_replacement_reporting() {
        let mut store = ModuleStateStore::new();
        assert!(!store.set(DemoState { value: 1 }));
        assert_eq!(store.get::<DemoState>(), Some(&DemoState { value: 1 }));
        assert!(store.set(DemoState { value: 2 }));
        assert_eq!(store.get::<DemoState>(), Some(&DemoState { value: 2 }));
    }

    #[test]
    fn get_mut_mutates_in_place() {
        let mut store = ModuleStateStore::new();
        store.set(DemoState { value: 1 });
        store.get_mut::<DemoState>().expect("state present").value += 10;
        assert_eq!(store.get::<DemoState>(), Some(&DemoState { value: 11 }));
    }

    #[test]
    fn remove_returns_uniquely_owned_value() {
        let mut store = ModuleStateStore::new();
        store.set(DemoState { value: 7 });
        assert_eq!(store.remove::<DemoState>(), Some(DemoState { value: 7 }));
        assert!(store.is_empty());
        assert!(store.get::<DemoState>().is_none());
        assert_eq!(store.remove::<DemoState>(), None);
    }

    #[test]
    fn distinct_types_do_not_collide() {
        let mut store = ModuleStateStore::new();
        store.set(DemoState { value: 1 });
        store.set(String::from("policy"));
        assert_eq!(store.get::<DemoState>(), Some(&DemoState { value: 1 }));
        assert_eq!(store.get::<String>(), Some(&String::from("policy")));
        assert!(!store.is_empty());
    }
}
