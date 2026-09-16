//! Generic lower-layer per-VM state table.
//!
//! [`ModuleStateStore`] is the single persistent, typed per-VM state table
//! owned by [`HostRuntime`](super::host_runtime::HostRuntime) and surfaced to
//! host extensions through
//! [`HostContext`](super::host_context::HostContext). It lives in the generic
//! VM layer so persistent storage is one generic primitive that does not
//! depend on [`HostContext`], [`HostModule`](super::host_context::HostModule),
//! builtins, or any adapter feature.
//!
//! The table owns two families of entries, both keyed by [`TypeId`] and both
//! surviving execution-scope reset / scope recycling for the lifetime of the
//! owning runtime/`Vm`:
//!
//! - **Typed module state** ([`Self::set`] / [`Self::get`] /
//!   [`Self::get_mut`] / [`Self::remove`]) — embedder-installed policy and
//!   configuration handed out as plain borrows.
//! - **Host-private state** (`ensure_host_state` /
//!   `host_state_ref` / `host_state_mut` / `set_host_state`) — the generic
//!   storage a host module owns for a per-VM cache or counter. It is *lazily*
//!   created from its [`HostStateProvider`] on first use, may be preconfigured
//!   before first use, is read through typed borrow guards, and reports every
//!   failure (conflicting provider, initialization error, borrow conflict) as
//!   a deterministic [`HostStateError`] naming the host function, the effect,
//!   and the concrete state type — never a panic.

use std::any::{Any, TypeId};
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::host_api::{
    HostState, HostStateLifetime, HostStateProvider, HostStateRequirement,
    HostStateRequirementError, dedupe_host_state_requirements,
};

/// The typed per-VM state table.
///
/// Persistent module state and host-private state live here and are exposed
/// through the host-context boundary. State is typed at compile time (keyed by
/// [`TypeId`]) and survives scope reset / scope recycling.
#[derive(Default)]
pub(crate) struct ModuleStateStore {
    entries: HashMap<TypeId, Box<dyn Any + Send>>,
    host_states: HashMap<TypeId, HostStateEntry>,
    providers: HashMap<TypeId, RegisteredProvider>,
    provider_keys: HashMap<&'static str, TypeId>,
}

/// One host-private state instance with its provider metadata.
struct HostStateEntry {
    key: &'static str,
    type_name: &'static str,
    cell: RefCell<Box<dyn Any + Send>>,
}

/// A registered (but not necessarily initialized) host-private state provider.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegisteredProvider {
    key: &'static str,
    type_name: &'static str,
    lifetime: HostStateLifetime,
}

/// Failure of a host-private state requirement or access.
///
/// Every variant names the host function, the effect, and the concrete state
/// type (or both conflicting types) so a host author can diagnose the failure
/// from the message alone. No access path panics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostStateError {
    /// The state requirement itself is unsatisfiable (conflicting provider).
    Requirement(HostStateRequirementError),
    /// Lazy initialization failed; `message` is the provider's own error.
    Initialization {
        function: String,
        effect: String,
        key: &'static str,
        type_name: &'static str,
        message: String,
    },
    /// The state is borrowed incompatibly (a second mutable borrow, or a
    /// mutable borrow while shared-borrowed).
    BorrowConflict {
        function: String,
        effect: String,
        key: &'static str,
        type_name: &'static str,
    },
    /// The state is reached without a prior `ensure_host_state` initialization.
    Missing {
        function: String,
        effect: String,
        key: &'static str,
        type_name: &'static str,
    },
    /// The requested state exists but holds a different concrete type.
    TypeMismatch {
        key: &'static str,
        /// Concrete type the state table holds.
        held_type: &'static str,
        /// Concrete type the caller requested.
        requested_type: &'static str,
    },
}

impl HostStateError {
    /// Requested host function, when the failure has a call site.
    pub fn function(&self) -> Option<&str> {
        match self {
            Self::Initialization { function, .. }
            | Self::BorrowConflict { function, .. }
            | Self::Missing { function, .. } => Some(function),
            Self::Requirement(_) | Self::TypeMismatch { .. } => None,
        }
    }

    /// Requested effect, when the failure has a call site.
    pub fn effect(&self) -> Option<&str> {
        match self {
            Self::Initialization { effect, .. }
            | Self::BorrowConflict { effect, .. }
            | Self::Missing { effect, .. } => Some(effect),
            Self::Requirement(_) | Self::TypeMismatch { .. } => None,
        }
    }

    /// Declared state key of the failing requirement, when known.
    pub fn key(&self) -> Option<&'static str> {
        match self {
            Self::Requirement(HostStateRequirementError::ProviderKeyConflict { key, .. })
            | Self::Requirement(HostStateRequirementError::ProviderConflict { key, .. })
            | Self::Initialization { key, .. }
            | Self::BorrowConflict { key, .. }
            | Self::Missing { key, .. }
            | Self::TypeMismatch { key, .. } => Some(key),
        }
    }
}

impl fmt::Display for HostStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requirement(error) => write!(formatter, "{error}"),
            Self::Initialization {
                function,
                effect,
                key,
                type_name,
                message,
            } => write!(
                formatter,
                "host state `{key}` initialization failed for `{type_name}` in `{function}` \
                 ({effect}): {message}"
            ),
            Self::BorrowConflict {
                function,
                effect,
                key,
                type_name,
            } => write!(
                formatter,
                "host state borrow conflict for `{type_name}` (`{key}`) in `{function}` \
                 ({effect}): state is already borrowed"
            ),
            Self::Missing {
                function,
                effect,
                key,
                type_name,
            } => write!(
                formatter,
                "host state `{key}` of type `{type_name}` is not initialized in `{function}` \
                 ({effect}): resolve the state before borrowing it"
            ),
            Self::TypeMismatch {
                key,
                held_type,
                requested_type,
            } => write!(
                formatter,
                "host state `{key}` holds `{held_type}` but `{requested_type}` was requested"
            ),
        }
    }
}

impl std::error::Error for HostStateError {}

impl From<HostStateRequirementError> for HostStateError {
    fn from(error: HostStateRequirementError) -> Self {
        Self::Requirement(error)
    }
}

/// Shared borrow guard of one host-private state instance.
#[derive(Debug)]
pub struct HostStateRef<'a, T: ?Sized> {
    borrow: Ref<'a, T>,
}

impl<'a, T: ?Sized> HostStateRef<'a, T> {
    pub(crate) fn new(borrow: Ref<'a, T>) -> Self {
        Self { borrow }
    }
}

impl<T: ?Sized> Deref for HostStateRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.borrow
    }
}

impl<T: ?Sized> AsRef<T> for HostStateRef<'_, T> {
    fn as_ref(&self) -> &T {
        &self.borrow
    }
}

/// Exclusive borrow guard of one host-private state instance.
#[derive(Debug)]
pub struct HostStateMut<'a, T: ?Sized> {
    borrow: RefMut<'a, T>,
}

impl<'a, T: ?Sized> HostStateMut<'a, T> {
    pub(crate) fn new(borrow: RefMut<'a, T>) -> Self {
        Self { borrow }
    }
}

impl<T: ?Sized> Deref for HostStateMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.borrow
    }
}

impl<T: ?Sized> DerefMut for HostStateMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.borrow
    }
}

impl<T: ?Sized> AsRef<T> for HostStateMut<'_, T> {
    fn as_ref(&self) -> &T {
        &self.borrow
    }
}

impl<T: ?Sized> AsMut<T> for HostStateMut<'_, T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.borrow
    }
}

impl ModuleStateStore {
    /// Creates an empty module-state store.
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            host_states: HashMap::new(),
            providers: HashMap::new(),
            provider_keys: HashMap::new(),
        }
    }

    // ---- typed module state -------------------------------------------------

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

    // ---- host-private state -------------------------------------------------

    /// Registers `T`'s provider and lazily creates the state on first use.
    ///
    /// Registration is idempotent for one state type. The canonical provider
    /// runs only when the state does not exist yet, so an embedder
    /// preconfiguration is never overwritten.
    pub(crate) fn ensure_host_state<T: HostState>(
        &mut self,
        function: &str,
        effect: &str,
    ) -> Result<(), HostStateError> {
        let provider = T::provider();
        self.ensure_host_state_provider(&provider, function, effect)
    }

    /// Registers an explicit provider descriptor without creating the value.
    pub(crate) fn register_host_state_provider(
        &mut self,
        provider: &HostStateProvider,
    ) -> Result<(), HostStateError> {
        self.validate_host_state_provider(provider)?;
        self.apply_host_state_provider(provider);
        Ok(())
    }

    /// Installs a requirement list transactionally, returning the deduplicated
    /// requirements.
    ///
    /// Identical providers deduplicate; the complete deduplicated list is
    /// validated against the current table before any provider is registered,
    /// so a conflicting requirement leaves the table unchanged.
    pub(crate) fn install_host_state_requirements(
        &mut self,
        requirements: &[HostStateRequirement],
    ) -> Result<Vec<HostStateRequirement>, HostStateError> {
        let merged = dedupe_host_state_requirements(requirements)?;
        for requirement in &merged {
            self.validate_host_state_provider(&requirement.provider)?;
        }
        for requirement in &merged {
            self.apply_host_state_provider(&requirement.provider);
        }
        Ok(merged)
    }

    /// Shared (read-only) access to host-private state of `T`.
    ///
    /// The state must already exist: callers resolve it with
    /// [`Self::ensure_host_state`] first (the generated hidden-state parameter
    /// path always does), which keeps lazy initialization outside the borrow
    /// guard and lets a single call hold several distinct state borrows.
    pub(crate) fn host_state_ref<T: HostState>(
        &self,
        provider: &HostStateProvider,
        function: &str,
        effect: &str,
    ) -> Result<HostStateRef<'_, T>, HostStateError> {
        self.validate_requested_type::<T>(provider)?;
        let cell = self.host_state_cell(provider, function, effect)?;
        let borrowed = cell
            .try_borrow()
            .map_err(|_| self.borrow_conflict(provider, function, effect))?;
        match Ref::filter_map(borrowed, |value| value.downcast_ref::<T>()) {
            Ok(typed) => Ok(HostStateRef::new(typed)),
            Err(_) => Err(self.type_mismatch::<T>(provider)),
        }
    }

    /// Exclusive (mutable) access to host-private state of `T`.
    pub(crate) fn host_state_mut<T: HostState>(
        &self,
        provider: &HostStateProvider,
        function: &str,
        effect: &str,
    ) -> Result<HostStateMut<'_, T>, HostStateError> {
        self.validate_requested_type::<T>(provider)?;
        let cell = self.host_state_cell(provider, function, effect)?;
        let borrowed = cell
            .try_borrow_mut()
            .map_err(|_| self.borrow_conflict(provider, function, effect))?;
        match RefMut::filter_map(borrowed, |value| value.downcast_mut::<T>()) {
            Ok(typed) => Ok(HostStateMut::new(typed)),
            Err(_) => Err(self.type_mismatch::<T>(provider)),
        }
    }

    /// Preconfigures host-private state of `T` before its first use.
    ///
    /// The lazy provider never overwrites a preconfigured value. Returns
    /// `true` when a previous value was replaced.
    pub(crate) fn set_host_state<T: HostState>(
        &mut self,
        state: T,
    ) -> Result<bool, HostStateError> {
        let provider = T::provider();
        self.register_host_state_provider(&provider)?;
        if let Some(entry) = self.host_states.get_mut(&provider.type_id()) {
            if entry.key != provider.key() {
                return Err(HostStateError::Requirement(
                    HostStateRequirementError::ProviderConflict {
                        key: entry.key,
                        type_name: entry.type_name,
                    },
                ));
            }
            let mut cell =
                entry
                    .cell
                    .try_borrow_mut()
                    .map_err(|_| HostStateError::BorrowConflict {
                        function: String::from("preconfigure"),
                        effect: String::from("state write"),
                        key: provider.key(),
                        type_name: provider.type_name(),
                    })?;
            let replaced = cell.downcast_ref::<T>().is_some();
            *cell = Box::new(state);
            return Ok(replaced);
        }
        self.host_states.insert(
            provider.type_id(),
            HostStateEntry {
                key: provider.key(),
                type_name: provider.type_name(),
                cell: RefCell::new(Box::new(state)),
            },
        );
        Ok(false)
    }

    /// Borrows host-private state of `T` without initializing it.
    pub(crate) fn host_state<T: HostState>(&self) -> Option<HostStateRef<'_, T>> {
        let entry = self.host_states.get(&TypeId::of::<T>())?;
        let borrowed = entry.cell.try_borrow().ok()?;
        let typed = Ref::filter_map(borrowed, |value| value.downcast_ref::<T>()).ok()?;
        Some(HostStateRef::new(typed))
    }

    /// Removes host-private state of `T`, returning the owned value.
    pub(crate) fn remove_host_state<T: HostState>(&mut self) -> Option<T> {
        let entry = self.host_states.remove(&TypeId::of::<T>())?;
        self.provider_keys.remove(entry.key);
        self.providers.remove(&TypeId::of::<T>());
        entry
            .cell
            .into_inner()
            .downcast::<T>()
            .ok()
            .map(|value| *value)
    }

    /// Returns `true` when no host-private state currently exists.
    pub(crate) fn is_host_state_empty(&self) -> bool {
        self.host_states.is_empty()
    }

    /// Read-only conflict check for one provider descriptor against the
    /// complete current table.
    ///
    /// A key owned by another concrete type, a concrete type already registered
    /// under a different key or lifetime, or an existing state instance under a
    /// different key all fail closed. Nothing is mutated, so a rejected batch
    /// leaves every table map exactly as it was.
    fn validate_host_state_provider(
        &self,
        provider: &HostStateProvider,
    ) -> Result<(), HostStateError> {
        if let Some(existing_type) = self.provider_keys.get(provider.key()).copied()
            && existing_type != provider.type_id()
        {
            return Err(HostStateError::Requirement(
                HostStateRequirementError::ProviderKeyConflict {
                    key: provider.key(),
                    existing_type: self.registered_type_name(existing_type),
                    conflicting_type: provider.type_name(),
                },
            ));
        }
        if let Some(existing) = self.providers.get(&provider.type_id())
            && (existing.key != provider.key() || existing.lifetime != provider.lifetime())
        {
            return Err(HostStateError::Requirement(
                HostStateRequirementError::ProviderConflict {
                    key: existing.key,
                    type_name: existing.type_name,
                },
            ));
        }
        if let Some(entry) = self.host_states.get(&provider.type_id())
            && entry.key != provider.key()
        {
            return Err(HostStateError::Requirement(
                HostStateRequirementError::ProviderConflict {
                    key: entry.key,
                    type_name: entry.type_name,
                },
            ));
        }
        Ok(())
    }

    /// Inserts one validated provider descriptor.
    ///
    /// Conflicts must be rejected by [`Self::validate_host_state_provider`]
    /// first; this step cannot fail, which is what makes a validated batch
    /// installable in one pass.
    fn apply_host_state_provider(&mut self, provider: &HostStateProvider) {
        self.provider_keys
            .insert(provider.key(), provider.type_id());
        self.providers.insert(
            provider.type_id(),
            RegisteredProvider {
                key: provider.key(),
                type_name: provider.type_name(),
                lifetime: provider.lifetime(),
            },
        );
    }

    /// Ensures `provider`'s state exists, running its initializer when needed.
    fn ensure_host_state_provider(
        &mut self,
        provider: &HostStateProvider,
        function: &str,
        effect: &str,
    ) -> Result<(), HostStateError> {
        self.register_host_state_provider(provider)?;
        if self.host_states.contains_key(&provider.type_id()) {
            return Ok(());
        }
        let value = provider
            .initialize()
            .map_err(|message| HostStateError::Initialization {
                function: function.to_string(),
                effect: effect.to_string(),
                key: provider.key(),
                type_name: provider.type_name(),
                message,
            })?;
        self.host_states.insert(
            provider.type_id(),
            HostStateEntry {
                key: provider.key(),
                type_name: provider.type_name(),
                cell: RefCell::new(value),
            },
        );
        Ok(())
    }

    /// Borrows the cell of `provider`'s host-private state.
    fn host_state_cell(
        &self,
        provider: &HostStateProvider,
        function: &str,
        effect: &str,
    ) -> Result<&RefCell<Box<dyn Any + Send>>, HostStateError> {
        let entry =
            self.host_states
                .get(&provider.type_id())
                .ok_or_else(|| HostStateError::Missing {
                    function: function.to_string(),
                    effect: effect.to_string(),
                    key: provider.key(),
                    type_name: provider.type_name(),
                })?;
        if entry.key != provider.key() {
            return Err(HostStateError::Requirement(
                HostStateRequirementError::ProviderConflict {
                    key: entry.key,
                    type_name: entry.type_name,
                },
            ));
        }
        Ok(&entry.cell)
    }

    /// Rejects a request whose concrete type is not the provider's type.
    fn validate_requested_type<T: HostState>(
        &self,
        provider: &HostStateProvider,
    ) -> Result<(), HostStateError> {
        if provider.type_id() == TypeId::of::<T>() {
            return Ok(());
        }
        Err(self.type_mismatch::<T>(provider))
    }

    /// Deterministic diagnostic for a concrete-type mismatch.
    ///
    /// `held_type` is the concrete type the provider's state actually holds and
    /// `requested_type` is the type the caller asked for.
    fn type_mismatch<T: HostState>(&self, provider: &HostStateProvider) -> HostStateError {
        HostStateError::TypeMismatch {
            key: provider.key(),
            held_type: provider.type_name(),
            requested_type: std::any::type_name::<T>(),
        }
    }

    fn borrow_conflict(
        &self,
        provider: &HostStateProvider,
        function: &str,
        effect: &str,
    ) -> HostStateError {
        HostStateError::BorrowConflict {
            function: function.to_string(),
            effect: effect.to_string(),
            key: provider.key(),
            type_name: provider.type_name(),
        }
    }

    /// Diagnostic type name recorded for one registered state type.
    fn registered_type_name(&self, type_id: TypeId) -> &'static str {
        self.providers
            .get(&type_id)
            .map(|registered| registered.type_name)
            .or_else(|| self.host_states.get(&type_id).map(|entry| entry.type_name))
            .unwrap_or("unknown host state type")
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use super::ModuleStateStore;
    use crate::host_api::{
        HostState, HostStateProvider, HostStateRequirement, HostStateRequirementError,
        dedupe_host_state_requirements,
    };
    use crate::vm::host_state::HostStateError;

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

    #[derive(Debug, Default, PartialEq)]
    struct LazyState {
        value: u64,
    }

    impl HostState for LazyState {
        const KEY: &'static str = "test.lazy_state";

        fn initialize() -> Result<Self, String> {
            Ok(Self::default())
        }
    }

    #[derive(Debug, PartialEq)]
    struct ImpostorState {
        value: u64,
    }

    impl HostState for ImpostorState {
        const KEY: &'static str = "test.lazy_state";

        fn initialize() -> Result<Self, String> {
            Ok(Self { value: 9 })
        }
    }

    #[test]
    fn host_state_initializes_lazily_and_idempotently() {
        let mut store = ModuleStateStore::new();
        assert!(store.is_host_state_empty());
        assert!(store.host_state::<LazyState>().is_none());

        store
            .ensure_host_state::<LazyState>("test::lazy", "state read LazyState")
            .expect("lazy initialization succeeds");
        assert_eq!(store.host_state::<LazyState>().expect("created").value, 0);

        store
            .ensure_host_state::<LazyState>("test::lazy", "state read LazyState")
            .expect("repeated resolution stays idempotent");
        assert_eq!(store.host_state::<LazyState>().expect("created").value, 0);
    }

    #[test]
    fn preconfigured_host_state_is_not_overwritten_by_the_provider() {
        let mut store = ModuleStateStore::new();
        assert!(!store.set_host_state(LazyState { value: 5 }).expect("set"));

        store
            .ensure_host_state::<LazyState>("test::lazy", "state read LazyState")
            .expect("preconfigured state resolves");
        assert_eq!(store.host_state::<LazyState>().expect("present").value, 5);

        assert!(
            store
                .set_host_state(LazyState { value: 6 })
                .expect("replace")
        );
        assert_eq!(store.host_state::<LazyState>().expect("present").value, 6);
    }

    #[test]
    fn conflicting_state_key_reports_both_types() {
        let mut store = ModuleStateStore::new();
        store.set_host_state(LazyState { value: 1 }).expect("set");

        let error = store
            .ensure_host_state::<ImpostorState>("test::impostor", "state read ImpostorState")
            .expect_err("a conflicting key must fail closed");
        assert_eq!(error.key(), Some("test.lazy_state"));
        let message = error.to_string();
        assert!(message.contains("LazyState"), "{message}");
        assert!(message.contains("ImpostorState"), "{message}");
    }

    #[test]
    fn borrow_conflicts_are_reported_without_panicking() {
        let mut store = ModuleStateStore::new();
        store.set_host_state(LazyState { value: 1 }).expect("set");

        let shared = store
            .host_state_ref::<LazyState>(
                &LazyState::provider(),
                "test::lazy",
                "state read LazyState",
            )
            .expect("shared borrow succeeds");
        assert_eq!(shared.value, 1);

        let error = store
            .host_state_mut::<LazyState>(
                &LazyState::provider(),
                "test::lazy",
                "state write LazyState",
            )
            .expect_err("mutable borrow while shared must fail closed");
        assert!(matches!(error, HostStateError::BorrowConflict { .. }));
        assert_eq!(error.function(), Some("test::lazy"));
        assert_eq!(error.effect(), Some("state write LazyState"));
    }

    #[test]
    fn borrowing_uninitialized_host_state_is_a_deterministic_error() {
        let store = ModuleStateStore::new();
        let error = store
            .host_state_ref::<LazyState>(
                &LazyState::provider(),
                "test::lazy",
                "state read LazyState",
            )
            .expect_err("borrowing before initialization must fail closed");
        assert!(matches!(error, HostStateError::Missing { .. }));
        assert!(error.to_string().contains("test.lazy_state"), "{error}");
    }

    #[test]
    fn removing_host_state_clears_its_provider() {
        let mut store = ModuleStateStore::new();
        store.set_host_state(LazyState { value: 3 }).expect("set");
        assert_eq!(
            store.remove_host_state::<LazyState>(),
            Some(LazyState { value: 3 })
        );
        assert_eq!(store.remove_host_state::<LazyState>(), None);
        assert!(store.is_host_state_empty());
        store
            .ensure_host_state::<LazyState>("test::lazy", "state read LazyState")
            .expect("provider can be registered again after removal");
    }

    #[derive(Debug, Default, PartialEq)]
    struct BatchState {
        value: u64,
    }

    impl HostState for BatchState {
        const KEY: &'static str = "test.batch_state";

        fn initialize() -> Result<Self, String> {
            Ok(Self::default())
        }
    }

    /// Sorted snapshot of the types that currently hold a state instance.
    fn state_types(store: &ModuleStateStore) -> Vec<TypeId> {
        let mut types: Vec<TypeId> = store.host_states.keys().copied().collect();
        types.sort();
        types
    }

    #[test]
    fn a_conflicting_batch_registers_no_provider_from_its_valid_prefix() {
        let mut store = ModuleStateStore::new();
        store
            .set_host_state(ImpostorState { value: 3 })
            .expect("the seeded conflict installs");

        let providers_before = store.providers.clone();
        let provider_keys_before = store.provider_keys.clone();
        let states_before = state_types(&store);

        let error = store
            .install_host_state_requirements(&[
                HostStateRequirement {
                    provider: BatchState::provider(),
                    write: false,
                },
                HostStateRequirement {
                    provider: LazyState::provider(),
                    write: true,
                },
            ])
            .expect_err("the later conflict must reject the whole batch");
        assert_eq!(error.key(), Some("test.lazy_state"));

        assert!(
            !store.providers.contains_key(&TypeId::of::<BatchState>()),
            "the valid prefix must not stay registered after a later conflict"
        );
        assert_eq!(
            store.providers, providers_before,
            "the provider map must be unchanged"
        );
        assert_eq!(
            store.provider_keys, provider_keys_before,
            "the provider key map must be unchanged"
        );
        assert_eq!(
            state_types(&store),
            states_before,
            "the state map must be unchanged"
        );

        // The rejected batch leaves the table retryable: the canonical install
        // of the valid prefix succeeds and resolves through its provider.
        let merged = store
            .install_host_state_requirements(&[HostStateRequirement {
                provider: BatchState::provider(),
                write: true,
            }])
            .expect("a canonical install must succeed after a rejected batch");
        assert_eq!(merged.len(), 1);
        assert!(
            merged[0].write,
            "read→write upgrades the retried requirement"
        );
        store
            .ensure_host_state::<BatchState>("test::batch", "state read BatchState")
            .expect("the canonical requirement resolves");
        assert_eq!(store.host_state::<BatchState>().expect("present").value, 0);
    }

    #[test]
    fn requirement_dedupe_identifies_states_by_concrete_type() {
        let read = HostStateRequirement {
            provider: HostStateProvider::with_diagnostic_name::<LazyState>(
                "test.lazy_state",
                "alias::ReadName",
            ),
            write: false,
        };
        let write = HostStateRequirement {
            provider: HostStateProvider::with_diagnostic_name::<LazyState>(
                "test.lazy_state",
                "alias::WriteName",
            ),
            write: true,
        };

        let merged = dedupe_host_state_requirements(&[read, write])
            .expect("one concrete type with one key must deduplicate");
        assert_eq!(merged.len(), 1);
        assert!(
            merged[0].write,
            "read→write upgrades the merged requirement"
        );
        assert_eq!(merged[0].provider.type_id(), TypeId::of::<LazyState>());
        assert_eq!(merged[0].key(), "test.lazy_state");
    }

    #[test]
    fn distinct_concrete_types_never_merge_on_equal_diagnostic_names() {
        let first = HostStateRequirement {
            provider: HostStateProvider::with_diagnostic_name::<LazyState>(
                "test.shared_key",
                "alias::SharedName",
            ),
            write: false,
        };
        let second = HostStateRequirement {
            provider: HostStateProvider::with_diagnostic_name::<ImpostorState>(
                "test.shared_key",
                "alias::SharedName",
            ),
            write: false,
        };

        let error = dedupe_host_state_requirements(&[first, second])
            .expect_err("distinct concrete types must not merge on an equal name");
        assert_eq!(
            error,
            HostStateRequirementError::ProviderKeyConflict {
                key: "test.shared_key",
                existing_type: "alias::SharedName",
                conflicting_type: "alias::SharedName",
            }
        );
    }

    #[test]
    fn one_concrete_type_with_two_keys_is_a_provider_conflict() {
        let first = HostStateRequirement {
            provider: HostStateProvider::with_diagnostic_name::<LazyState>(
                "test.first_key",
                "alias::Lazy",
            ),
            write: false,
        };
        let second = HostStateRequirement {
            provider: HostStateProvider::with_diagnostic_name::<LazyState>(
                "test.second_key",
                "alias::Lazy",
            ),
            write: false,
        };

        let error = dedupe_host_state_requirements(&[first, second])
            .expect_err("one concrete type cannot declare two state keys");
        assert_eq!(
            error,
            HostStateRequirementError::ProviderConflict {
                key: "test.first_key",
                type_name: "alias::Lazy",
            }
        );
    }

    #[test]
    fn type_mismatch_reports_provider_type_before_requested_type() {
        let store = ModuleStateStore::new();
        let provider = ImpostorState::provider();

        let error = store
            .host_state_ref::<LazyState>(&provider, "test::lazy", "state read LazyState")
            .expect_err("a provider for another concrete type must fail closed");
        assert_eq!(error.key(), Some("test.lazy_state"));

        let message = error.to_string();
        assert_eq!(
            message,
            format!(
                "host state `test.lazy_state` holds `{}` but `{}` was requested",
                std::any::type_name::<ImpostorState>(),
                std::any::type_name::<LazyState>(),
            ),
            "the diagnostic must name the provider-held type first"
        );
        match &error {
            HostStateError::TypeMismatch {
                key,
                held_type,
                requested_type,
            } => {
                assert_eq!(*key, "test.lazy_state");
                assert_eq!(*held_type, std::any::type_name::<ImpostorState>());
                assert_eq!(*requested_type, std::any::type_name::<LazyState>());
            }
            other => panic!("expected a concrete-type mismatch, got {other}"),
        }

        let error = store
            .host_state_mut::<LazyState>(&provider, "test::lazy", "state write LazyState")
            .expect_err("the mutable path reports the same mismatch");
        assert_eq!(error.to_string(), message);
    }
}
