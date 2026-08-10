use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::vm::Value;

use super::cancellation::CancellationReason;
use super::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};

pub const DEFAULT_MAX_RESOURCES: usize = 1024;

const HANDLE_TYPE_BITS: u64 = 8;
const HANDLE_GENERATION_BITS: u64 = 17;
const HANDLE_SLOT_BITS: u64 = 18;
const HANDLE_ARENA_BITS: u64 = 63 - HANDLE_TYPE_BITS - HANDLE_GENERATION_BITS - HANDLE_SLOT_BITS;

const HANDLE_TYPE_SHIFT: u64 = 0;
const HANDLE_GENERATION_SHIFT: u64 = HANDLE_TYPE_BITS;
const HANDLE_SLOT_SHIFT: u64 = HANDLE_GENERATION_SHIFT + HANDLE_GENERATION_BITS;
const HANDLE_ARENA_SHIFT: u64 = HANDLE_SLOT_SHIFT + HANDLE_SLOT_BITS;

const HANDLE_TYPE_MASK: u64 = (1 << HANDLE_TYPE_BITS) - 1;
const HANDLE_GENERATION_MASK: u64 = (1 << HANDLE_GENERATION_BITS) - 1;
const HANDLE_SLOT_MASK: u64 = (1 << HANDLE_SLOT_BITS) - 1;
const HANDLE_ARENA_MASK: u64 = (1 << HANDLE_ARENA_BITS) - 1;

/// Process-wide monotonic arena identity source. Arena identities are not
/// recycled, so a handle from a dropped VM cannot resolve in a later VM.
static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(1);

/// Stable resource type identity carried by every opaque handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceTypeId(u16);

impl ResourceTypeId {
    pub const IO_FILE: Self = Self(1);
    #[cfg_attr(not(feature = "http-client"), allow(dead_code))]
    pub const HTTP_REQUEST: Self = Self(3);
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub const SQLITE_CONNECTION: Self = Self(5);
    pub const CALLBACK: Self = Self(6);

    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// A positive VM integer identifying one typed resource without exposing it.
///
/// The token carries arena, slot, generation, and resource-type identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceHandle(u64);

impl ResourceHandle {
    pub fn as_value(self) -> Value {
        Value::Int(self.0 as i64)
    }

    pub fn from_value(value: &Value) -> RuntimeResult<Self> {
        let Value::Int(raw) = value else {
            return Err(invalid_handle("resource handle must be an integer token"));
        };
        if *raw <= 0 {
            return Err(invalid_handle("resource handle must be a positive token"));
        }
        Self::from_encoded(*raw as u64)
    }

    pub const fn resource_type(self) -> ResourceTypeId {
        ResourceTypeId(((self.0 >> HANDLE_TYPE_SHIFT) & HANDLE_TYPE_MASK) as u16)
    }

    const fn arena_id(self) -> u64 {
        (self.0 >> HANDLE_ARENA_SHIFT) & HANDLE_ARENA_MASK
    }

    const fn slot_identity(self) -> u64 {
        (self.0 >> HANDLE_SLOT_SHIFT) & HANDLE_SLOT_MASK
    }

    const fn generation(self) -> u64 {
        (self.0 >> HANDLE_GENERATION_SHIFT) & HANDLE_GENERATION_MASK
    }

    fn slot_index(self) -> RuntimeResult<usize> {
        usize::try_from(self.slot_identity() - 1)
            .map_err(|_| invalid_handle("resource handle slot is out of range"))
    }

    fn from_encoded(encoded: u64) -> RuntimeResult<Self> {
        let handle = Self(encoded);
        if encoded == 0
            || encoded > i64::MAX as u64
            || handle.arena_id() == 0
            || handle.slot_identity() == 0
            || handle.generation() == 0
            || handle.resource_type().raw() == 0
        {
            return Err(invalid_handle(
                "resource handle token has an invalid encoding",
            ));
        }
        Ok(handle)
    }

    fn encode(
        arena_id: u64,
        slot_index: usize,
        generation: u64,
        resource_type: ResourceTypeId,
    ) -> RuntimeResult<Self> {
        let slot_identity = u64::try_from(slot_index)
            .ok()
            .and_then(|slot| slot.checked_add(1))
            .ok_or_else(|| invalid_handle("resource slot identity overflowed"))?;
        if arena_id == 0
            || arena_id > HANDLE_ARENA_MASK
            || slot_identity > HANDLE_SLOT_MASK
            || generation == 0
            || generation > HANDLE_GENERATION_MASK
            || resource_type.raw() == 0
            || u64::from(resource_type.raw()) > HANDLE_TYPE_MASK
        {
            return Err(invalid_handle(
                "resource handle components are out of range",
            ));
        }
        let encoded = (arena_id << HANDLE_ARENA_SHIFT)
            | (slot_identity << HANDLE_SLOT_SHIFT)
            | (generation << HANDLE_GENERATION_SHIFT)
            | (u64::from(resource_type.raw()) << HANDLE_TYPE_SHIFT);
        Self::from_encoded(encoded)
    }
}

type ErasedResource = Box<dyn Any + Send>;
type ResourceCleanup =
    Box<dyn FnOnce(ErasedResource, CancellationReason) -> RuntimeResult<()> + Send + 'static>;

struct ResourceSlot {
    generation: u32,
    resource_type: ResourceTypeId,
    value: Option<ErasedResource>,
    cleanup: Option<ResourceCleanup>,
}

/// VM-local bounded arena for typed opaque host resources.
pub struct ResourceArena {
    arena_id: u64,
    max_entries: usize,
    slots: Vec<ResourceSlot>,
    vacant_slots: Vec<usize>,
    active_entries: usize,
}

impl ResourceArena {
    pub fn with_limit(max_entries: usize) -> RuntimeResult<Self> {
        if max_entries == 0 || max_entries > HANDLE_SLOT_MASK as usize {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidConfiguration,
                "resource::arena",
                format!(
                    "resource arena capacity must be between 1 and {}",
                    HANDLE_SLOT_MASK
                ),
            )
            .with_limit(HANDLE_SLOT_MASK as usize));
        }
        let arena_id = NEXT_ARENA_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |arena_id| {
                (arena_id <= HANDLE_ARENA_MASK).then_some(arena_id + 1)
            })
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::ResourceIdExhausted,
                    "resource::arena",
                    "resource arena identity space is exhausted",
                )
            })?;
        Ok(Self {
            arena_id,
            max_entries,
            slots: Vec::new(),
            vacant_slots: Vec::new(),
            active_entries: 0,
        })
    }

    pub fn insert<T>(
        &mut self,
        resource_type: ResourceTypeId,
        value: T,
    ) -> RuntimeResult<ResourceHandle>
    where
        T: Any + Send + 'static,
    {
        self.allocate(resource_type, Box::new(value), None)
    }

    pub fn insert_with_cleanup<T, F>(
        &mut self,
        resource_type: ResourceTypeId,
        value: T,
        cleanup: F,
    ) -> RuntimeResult<ResourceHandle>
    where
        T: Any + Send + 'static,
        F: FnOnce(T, CancellationReason) -> RuntimeResult<()> + Send + 'static,
    {
        let erased_cleanup: ResourceCleanup = Box::new(move |value, reason| {
            let value = value.downcast::<T>().map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::ResourceTypeMismatch,
                    "resource::cleanup",
                    "resource cleanup received the wrong concrete type",
                )
            })?;
            cleanup(*value, reason)
        });
        self.allocate(resource_type, Box::new(value), Some(erased_cleanup))
    }

    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub fn count_type(&self, resource_type: ResourceTypeId) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.resource_type == resource_type && slot.value.is_some())
            .count()
    }

    #[cfg(feature = "sqlite")]
    pub fn handles_of_type(&self, resource_type: ResourceTypeId) -> Vec<ResourceHandle> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.resource_type == resource_type && slot.value.is_some())
            .filter_map(|(slot_index, slot)| {
                ResourceHandle::encode(
                    self.arena_id,
                    slot_index,
                    u64::from(slot.generation),
                    slot.resource_type,
                )
                .ok()
            })
            .collect()
    }

    pub fn get<T>(&self, handle: ResourceHandle, expected_type: ResourceTypeId) -> RuntimeResult<&T>
    where
        T: Any + Send + 'static,
    {
        self.active_slot(handle, expected_type)?
            .value
            .as_ref()
            .and_then(|value| value.downcast_ref::<T>())
            .ok_or_else(|| type_mismatch(handle, expected_type))
    }

    pub fn get_mut<T>(
        &mut self,
        handle: ResourceHandle,
        expected_type: ResourceTypeId,
    ) -> RuntimeResult<&mut T>
    where
        T: Any + Send + 'static,
    {
        self.active_slot_mut(handle, expected_type)?
            .value
            .as_mut()
            .and_then(|value| value.downcast_mut::<T>())
            .ok_or_else(|| type_mismatch(handle, expected_type))
    }

    pub fn close(
        &mut self,
        handle: ResourceHandle,
        reason: CancellationReason,
    ) -> RuntimeResult<CloseStatus> {
        let slot_index = self.validate_handle_identity(handle)?;
        let (value, cleanup, reusable) = {
            let slot = &mut self.slots[slot_index];
            validate_slot_identity(slot, handle)?;
            if slot.resource_type != handle.resource_type() {
                return Err(type_mismatch(handle, slot.resource_type));
            }
            let Some(value) = slot.value.take() else {
                return Ok(CloseStatus::AlreadyClosed);
            };
            self.active_entries -= 1;
            (
                value,
                slot.cleanup.take(),
                u64::from(slot.generation) < HANDLE_GENERATION_MASK,
            )
        };
        if reusable {
            self.vacant_slots.push(slot_index);
        }
        let result = if let Some(cleanup) = cleanup {
            cleanup(value, reason)
        } else {
            drop(value);
            Ok(())
        };
        result.map(|()| CloseStatus::Closed).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::ResourceCleanupFailed,
                "resource::close",
                error.to_string(),
            )
            .with_value(handle.0)
        })
    }

    pub fn close_all(&mut self, reason: CancellationReason) -> RuntimeResult<usize> {
        let handles = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot_index, slot)| {
                slot.value.as_ref().map(|_| {
                    ResourceHandle::encode(
                        self.arena_id,
                        slot_index,
                        u64::from(slot.generation),
                        slot.resource_type,
                    )
                    .expect("active resource slot must have an encodable handle")
                })
            })
            .collect::<Vec<_>>();
        let mut closed = 0;
        let mut first_error = None;
        for handle in handles {
            match self.close(handle, reason) {
                Ok(CloseStatus::Closed) => closed += 1,
                Ok(CloseStatus::AlreadyClosed) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(closed),
        }
    }

    fn allocate(
        &mut self,
        resource_type: ResourceTypeId,
        value: ErasedResource,
        cleanup: Option<ResourceCleanup>,
    ) -> RuntimeResult<ResourceHandle> {
        if resource_type.raw() == 0 || u64::from(resource_type.raw()) > HANDLE_TYPE_MASK {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ResourceTypeMismatch,
                "resource::insert",
                "resource type id is outside the handle encoding range",
            ));
        }
        if self.active_entries >= self.max_entries {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ResourceLimitExceeded,
                "resource::insert",
                "resource arena capacity has been reached",
            )
            .with_limit(self.max_entries));
        }

        let (slot_index, generation) = if let Some(slot_index) = self.vacant_slots.pop() {
            let slot = &mut self.slots[slot_index];
            let generation = slot
                .generation
                .checked_add(1)
                .filter(|generation| u64::from(*generation) <= HANDLE_GENERATION_MASK)
                .expect("only reusable resource generations enter the vacant list");
            slot.generation = generation;
            slot.resource_type = resource_type;
            slot.value = Some(value);
            slot.cleanup = cleanup;
            (slot_index, generation)
        } else {
            if self.slots.len() >= self.max_entries {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::ResourceIdExhausted,
                    "resource::insert",
                    "resource slot generation space is exhausted",
                ));
            }
            let slot_index = self.slots.len();
            let generation = 1;
            self.slots.push(ResourceSlot {
                generation,
                resource_type,
                value: Some(value),
                cleanup,
            });
            (slot_index, generation)
        };
        self.active_entries += 1;
        ResourceHandle::encode(
            self.arena_id,
            slot_index,
            u64::from(generation),
            resource_type,
        )
    }

    fn validate_handle_identity(&self, handle: ResourceHandle) -> RuntimeResult<usize> {
        if handle.arena_id() != self.arena_id {
            return Err(wrong_arena(handle));
        }
        let slot_index = handle.slot_index()?;
        if slot_index >= self.slots.len() {
            return Err(stale_handle(handle));
        }
        Ok(slot_index)
    }

    fn active_slot(
        &self,
        handle: ResourceHandle,
        expected_type: ResourceTypeId,
    ) -> RuntimeResult<&ResourceSlot> {
        validate_type(handle, expected_type)?;
        let slot_index = self.validate_handle_identity(handle)?;
        let slot = &self.slots[slot_index];
        validate_slot(slot, handle, expected_type)?;
        Ok(slot)
    }

    fn active_slot_mut(
        &mut self,
        handle: ResourceHandle,
        expected_type: ResourceTypeId,
    ) -> RuntimeResult<&mut ResourceSlot> {
        validate_type(handle, expected_type)?;
        let slot_index = self.validate_handle_identity(handle)?;
        let slot = &mut self.slots[slot_index];
        validate_slot(slot, handle, expected_type)?;
        Ok(slot)
    }
}

impl Default for ResourceArena {
    fn default() -> Self {
        Self::with_limit(DEFAULT_MAX_RESOURCES)
            .expect("default resource arena configuration should be valid")
    }
}

impl Drop for ResourceArena {
    fn drop(&mut self) {
        let _ = self.close_all(CancellationReason::VmReset);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseStatus {
    Closed,
    AlreadyClosed,
}

fn validate_type(handle: ResourceHandle, expected_type: ResourceTypeId) -> RuntimeResult<()> {
    if handle.resource_type() != expected_type {
        return Err(type_mismatch(handle, expected_type));
    }
    Ok(())
}

fn validate_slot_identity(slot: &ResourceSlot, handle: ResourceHandle) -> RuntimeResult<()> {
    if u64::from(slot.generation) != handle.generation() {
        return Err(stale_handle(handle));
    }
    Ok(())
}

fn validate_slot(
    slot: &ResourceSlot,
    handle: ResourceHandle,
    expected_type: ResourceTypeId,
) -> RuntimeResult<()> {
    validate_slot_identity(slot, handle)?;
    if slot.resource_type != expected_type {
        return Err(type_mismatch(handle, expected_type));
    }
    if slot.value.is_none() {
        return Err(already_closed_error(handle));
    }
    Ok(())
}

fn invalid_handle(message: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::InvalidResourceHandle,
        "resource::handle",
        message,
    )
}

fn wrong_arena(handle: ResourceHandle) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ResourceHandleWrongTable,
        "resource::handle",
        "resource handle does not belong to this VM arena",
    )
    .with_value(handle.0)
}

fn stale_handle(handle: ResourceHandle) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ResourceStale,
        "resource::handle",
        "resource handle refers to a stale slot generation",
    )
    .with_value(handle.0)
}

fn already_closed_error(handle: ResourceHandle) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ResourceAlreadyClosed,
        "resource::handle",
        "resource is already closed",
    )
    .with_value(handle.0)
}

fn type_mismatch(handle: ResourceHandle, expected: ResourceTypeId) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ResourceTypeMismatch,
        "resource::handle",
        format!(
            "resource type {} does not match expected type {}",
            handle.resource_type().raw(),
            expected.raw()
        ),
    )
    .with_value(handle.0)
}

#[cfg(test)]
mod tests {
    use super::{CancellationReason, ResourceArena, ResourceTypeId};

    #[test]
    fn vacant_slot_reuse_increments_the_generation() {
        let mut arena = ResourceArena::with_limit(1).expect("arena should be valid");
        let first = arena
            .insert(ResourceTypeId::IO_FILE, 1_u8)
            .expect("first resource should be inserted");
        assert_eq!(
            arena
                .close(first, CancellationReason::ResourceClosed)
                .expect("first resource should close"),
            super::CloseStatus::Closed
        );

        let replacement = arena
            .insert(ResourceTypeId::IO_FILE, 2_u8)
            .expect("vacant slot should be reused");

        assert_eq!(replacement.slot_identity(), first.slot_identity());
        assert_eq!(replacement.generation(), first.generation() + 1);
    }
}
