//! Typed, host-agnostic resource handles.
//!
//! A [`ResourceHandle`] is an opaque token that encodes exactly three
//! identities, with no domain resource class information:
//!
//! ```text
//! arena / scope identity | slot index | generation
//! ```
//!
//! The arena identity binds a handle to one [`ResourceTable`](super::table::ResourceTable)
//! (and therefore to the execution scope that owns that table). The slot index
//! locates the entry, and the generation rejects handles that outlive a
//! slot-reuse. Concrete resource type is checked at borrow time with a
//! [`std::any::TypeId`], never by discarding space in the handle.
//!
//! [`Resource<T>`] is a type-marked token that host code keeps while it talks
//! about a particular resource. It is `Copy`, but it is only a capability
//! token: duplicating the token duplicates the name, not ownership of the
//! underlying resource, whose lifetime is governed by the table.

use std::marker::PhantomData;

use crate::bytecode::Value;

use super::error::{ResourceError, ResourceErrorCode, ResourceResult};

/// Default bounded capacity of a resource table.
pub const DEFAULT_MAX_RESOURCES: usize = 1024;

const HANDLE_GENERATION_BITS: u64 = 25;
const HANDLE_SLOT_BITS: u64 = 18;
const HANDLE_ARENA_BITS: u64 = 63 - HANDLE_GENERATION_BITS - HANDLE_SLOT_BITS;

const HANDLE_GENERATION_SHIFT: u64 = 0;
const HANDLE_SLOT_SHIFT: u64 = HANDLE_GENERATION_SHIFT + HANDLE_GENERATION_BITS;
const HANDLE_ARENA_SHIFT: u64 = HANDLE_SLOT_SHIFT + HANDLE_SLOT_BITS;

const HANDLE_GENERATION_MASK: u64 = (1 << HANDLE_GENERATION_BITS) - 1;
const HANDLE_SLOT_MASK: u64 = (1 << HANDLE_SLOT_BITS) - 1;
const HANDLE_ARENA_MASK: u64 = (1 << HANDLE_ARENA_BITS) - 1;

/// Hard ceiling on resident slots, derived from the handle encoding.
pub(crate) const MAX_RESOURCE_SLOTS: usize = HANDLE_SLOT_MASK as usize;

/// Largest valid arena identity.
pub(crate) const MAX_HANDLE_ARENA_ID: u64 = HANDLE_ARENA_MASK;

/// Largest valid slot generation.
pub(crate) const MAX_HANDLE_GENERATION: u64 = HANDLE_GENERATION_MASK;

/// Raw opaque resource token passed across the host boundary.
///
/// The token is a positive signed VM integer. Zero and any encoding field
/// being zero are invalid, so the token space never aliases a reserved value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ResourceHandle(u64);

impl ResourceHandle {
    /// Converts the handle into a positive VM integer.
    pub fn as_value(self) -> Value {
        Value::Int(self.raw() as i64)
    }

    /// Parses a positive VM integer into a handle.
    ///
    /// The raw bytes are not trusted: the encoding is validated, so numbers
    /// that happen to pass the range checks (including zero and non-positive
    /// values) are rejected with a typed [`ResourceErrorCode::InvalidResourceHandle`].
    pub fn from_value(value: &Value) -> ResourceResult<Self> {
        let Value::Int(raw) = value else {
            return Err(invalid_handle("resource handle must be an integer token"));
        };
        Self::from_raw(*raw as u64)
    }

    /// The raw `u64` encoding.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Rebuilds a handle from the raw encoding, validating that no reserved or
    /// truncated component leaked through.
    pub fn from_raw(raw: u64) -> ResourceResult<Self> {
        if raw == 0 || raw > i64::MAX as u64 {
            return Err(invalid_handle(
                "resource handle token must be a positive signed integer",
            ));
        }
        let handle = Self(raw);
        if handle.arena_id() == 0 || handle.slot_identity() == 0 || handle.generation() == 0 {
            return Err(invalid_handle(
                "resource handle token has an invalid encoding",
            ));
        }
        Ok(handle)
    }

    /// Process-unique arena / scope identity, never recycled.
    pub(crate) const fn arena_id(self) -> u64 {
        (self.0 >> HANDLE_ARENA_SHIFT) & HANDLE_ARENA_MASK
    }

    /// Generation for the slot, advanced on every reuse.
    pub fn generation(self) -> u64 {
        (self.0 >> HANDLE_GENERATION_SHIFT) & HANDLE_GENERATION_MASK
    }

    /// Zero-based slot index.
    pub fn slot_index(self) -> ResourceResult<usize> {
        usize::try_from(self.slot_identity() - 1)
            .map_err(|_| invalid_handle("resource handle slot is out of range"))
    }

    const fn slot_identity(self) -> u64 {
        (self.0 >> HANDLE_SLOT_SHIFT) & HANDLE_SLOT_MASK
    }

    pub(crate) fn encode(arena_id: u64, slot_index: usize, generation: u64) -> Option<Self> {
        let slot_identity = u64::try_from(slot_index).ok()?.checked_add(1)?;
        if arena_id == 0
            || arena_id > HANDLE_ARENA_MASK
            || slot_identity == 0
            || slot_identity > HANDLE_SLOT_MASK
            || generation == 0
            || generation > HANDLE_GENERATION_MASK
        {
            return None;
        }
        Some(Self(
            (arena_id << HANDLE_ARENA_SHIFT)
                | (slot_identity << HANDLE_SLOT_SHIFT)
                | (generation << HANDLE_GENERATION_SHIFT),
        ))
    }
}

/// A type-marked capability token over one resource.
///
/// `Resource<T>` is `Copy` and cheap; it is a key into a table, not an owner.
/// The `PhantomData<fn() -> T>` marker keeps the token covariant and lets it be
/// `Copy`/`Send`/`Sync` *regardless* of whether `T` itself is, while still
/// carrying the concrete type for borrow-time validation. The trait impls are
/// hand-written (instead of derived) precisely so no `T: Copy`/`T: Clone` etc.
/// bound leaks onto the token.
pub struct Resource<T> {
    raw: ResourceHandle,
    marker: PhantomData<fn() -> T>,
}

impl<T> Resource<T> {
    /// Builds a typed token over a validated raw handle.
    ///
    /// This is the reclaim path for host code that carries a raw integer token
    /// (for example one stored inside a script value) and wants typed access.
    pub fn from_handle(raw: ResourceHandle) -> Self {
        Self {
            raw,
            marker: PhantomData,
        }
    }

    /// The underlying opaque handle.
    pub fn handle(&self) -> ResourceHandle {
        self.raw
    }

    /// Consumes the token and returns the raw handle.
    pub const fn into_handle(self) -> ResourceHandle {
        self.raw
    }
}

#[allow(clippy::non_canonical_clone_impl)]
impl<T> Clone for Resource<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw,
            marker: PhantomData,
        }
    }
}

impl<T> Copy for Resource<T> {}

impl<T> PartialEq for Resource<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Resource<T> {}

impl<T> PartialOrd for Resource<T> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Resource<T> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> core::hash::Hash for Resource<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> core::fmt::Debug for Resource<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Resource").field(&self.raw).finish()
    }
}

/// An immutable borrow of one resource bound to a single host call.
///
/// The handle makes the association explicit and keeps the borrow alive for a
/// controlled duration; it is `Copy` (mirroring the token) and is not meant to
/// live across a yield or poll boundary.
pub struct ResourceRef<'a, T> {
    handle: ResourceHandle,
    value: &'a T,
}

impl<'a, T> ResourceRef<'a, T> {
    pub(crate) fn new(handle: ResourceHandle, value: &'a T) -> Self {
        Self { handle, value }
    }

    pub fn handle(&self) -> ResourceHandle {
        self.handle
    }

    pub fn get(&self) -> &'a T {
        self.value
    }
}

#[allow(clippy::non_canonical_clone_impl)]
impl<T> Clone for ResourceRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ResourceRef<'_, T> {}

impl<T> core::fmt::Debug for ResourceRef<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResourceRef")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl<T> core::ops::Deref for ResourceRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

/// A mutable borrow of a [`Resource<T>`], scoped to a single host call.
pub struct ResourceMut<'a, T> {
    handle: ResourceHandle,
    value: &'a mut T,
}

impl<'a, T> ResourceMut<'a, T> {
    pub(crate) fn new(handle: ResourceHandle, value: &'a mut T) -> Self {
        Self { handle, value }
    }

    pub fn handle(&self) -> ResourceHandle {
        self.handle
    }

    pub fn get(&mut self) -> &mut T {
        self.value
    }
}

impl<T> core::fmt::Debug for ResourceMut<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResourceMut")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl<T> core::ops::Deref for ResourceMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> core::ops::DerefMut for ResourceMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

fn invalid_handle(message: &'static str) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::InvalidResourceHandle,
        "resource::handle",
        message,
    )
}
