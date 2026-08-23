//! VM-owned packed operation identifiers.
//!
//! An [`OperationId`] is an opaque 63-bit token that *packs* the three
//! identifiers that uniquely address an in-flight operation in this VM:
//!
//! * a **registry tag** identifying which [`registry::OperationRegistry`]
//!   owns the id (allocated by [`allocate_registry_tag`]);
//! * a one-based **slot identity** selecting an entry inside that registry;
//! * a **generation** that distinguishes successive occupants of the same
//!   slot.
//!
//! Packing the three fields into a single `u64` keeps the id copyable and
//! passable across a dynamic host call as the lone capability token, while
//! still allowing per-field validation and recovery.
//!
//! ## Bit layout (63-bit positive)
//!
//! The top (sign) bit is clear so the id is a positive `i64`. The remaining
//! 63 bits are split into three contiguous fields, high to low:
//!
//! ```text
//!  63        43 42        22 21          0
//! |<- tag:20 ->|<- slot:21 ->|<- gen:22 ->|
//!   MSB                              LSB
//! ```
//!
//! Fields are one-based where noted (slot identity, tag, generation all start
//! at `1`); a field value of `0` is never a valid id.

use std::sync::atomic::{AtomicU64, Ordering};

use super::error::{OperationError, OperationErrorCode, OperationResult};

/// Width (bits) of the registry-tag field.
const REG_TAG_BITS: u32 = 20;
/// Width (bits) of the slot-identity field.
const SLOT_BITS: u32 = 21;
/// Width (bits) of the generation field.
const GEN_BITS: u32 = 22;

/// Shift up to the registry-tag field.
const REG_TAG_SHIFT: u32 = SLOT_BITS + GEN_BITS;
/// Shift up to the slot-identity field.
const SLOT_SHIFT: u32 = GEN_BITS;
/// The generation resides in the low bits.
const GEN_SHIFT: u32 = 0;

/// Reserved top (sign) bit; must always be clear in a valid raw id.
const SIGN_MASK: u64 = 1u64 << 63;
/// Field mask for the registry tag.
const REG_TAG_MASK: u64 = ((1u64 << REG_TAG_BITS) - 1) << REG_TAG_SHIFT;
/// Field mask for the slot identity.
const SLOT_MASK: u64 = ((1u64 << SLOT_BITS) - 1) << SLOT_SHIFT;
/// Field mask for the generation.
const GEN_MASK: u64 = ((1u64 << GEN_BITS) - 1) << GEN_SHIFT;

/// Maximum registry tag (inclusive); tag `0` is reserved/invalid.
pub(crate) const MAX_REGISTRY_TAG: u64 = (1u64 << REG_TAG_BITS) - 1;
/// Maximum one-based slot identity (inclusive).
pub(super) const MAX_SLOT_IDENTITY: u64 = (1u64 << SLOT_BITS) - 1;
/// Maximum generation (inclusive); generation `0` is reserved/invalid.
pub(super) const MAX_GENERATION: u64 = (1u64 << GEN_BITS) - 1;

/// Process-global allocator of registry tags.
///
/// Tags start at `1`, are handed out monotonically, are never reused, and
/// eventually saturate at [`MAX_REGISTRY_TAG`]; the call immediately after
/// the maximum is handed out fails with `OperationRegistryTagExhausted`.
static NEXT_REGISTRY_TAG: AtomicU64 = AtomicU64::new(1);

/// Test-only, per-thread registry-tag source override.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::cell::Cell;
    use std::sync::atomic::AtomicU64;

    thread_local! {
        static REGISTRY_TAG_SOURCE: Cell<Option<&'static AtomicU64>> = const { Cell::new(None) };
    }

    pub(crate) fn source() -> Option<&'static AtomicU64> {
        REGISTRY_TAG_SOURCE.with(|cell| cell.get())
    }

    /// Installs a private tag counter for the current thread until drop.
    pub(crate) struct ScopedRegistryTagSource;

    impl ScopedRegistryTagSource {
        pub(crate) fn install(counter: &'static AtomicU64) -> Self {
            REGISTRY_TAG_SOURCE.with(|cell| {
                assert!(
                    cell.get().is_none(),
                    "nested registry tag source override is unsupported"
                );
                cell.set(Some(counter));
            });
            Self
        }
    }

    impl Drop for ScopedRegistryTagSource {
        fn drop(&mut self) {
            REGISTRY_TAG_SOURCE.with(|cell| cell.set(None));
        }
    }
}

/// Opaque, packed VM operation identifier.
///
/// Represents the (registry tag, slot identity, generation) triple as a
/// single positive 63-bit token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(u64);

impl OperationId {
    /// Validates and decodes a raw packed id.
    ///
    /// Rejects a zero raw value, a set sign bit, a zero/out-of-range
    /// registry tag, a zero slot identity, and a zero generation, each with
    /// [`OperationErrorCode::InvalidOperationId`] carrying the offending
    /// raw value as its `value` payload.
    pub fn from_raw(raw: u64) -> OperationResult<Self> {
        let invalid = || {
            OperationError::new(
                OperationErrorCode::InvalidOperationId,
                "vm::operation",
                "invalid packed operation id",
            )
            .with_value(raw)
        };

        if raw == 0 || (raw & SIGN_MASK) != 0 {
            return Err(invalid());
        }

        let tag = (raw & REG_TAG_MASK) >> REG_TAG_SHIFT;
        let slot_identity = (raw & SLOT_MASK) >> SLOT_SHIFT;
        let generation = (raw & GEN_MASK) >> GEN_SHIFT;

        if tag == 0 || tag > MAX_REGISTRY_TAG {
            return Err(invalid());
        }
        if slot_identity == 0 || slot_identity > MAX_SLOT_IDENTITY {
            return Err(invalid());
        }
        if generation == 0 || generation > MAX_GENERATION {
            return Err(invalid());
        }

        Ok(Self(raw))
    }

    /// The raw packed id, safe to pass across a dynamic host call where the
    /// id is the only capability token the script holds.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The owning registry tag (one-based).
    pub(super) const fn registry_tag(self) -> u64 {
        (self.0 & REG_TAG_MASK) >> REG_TAG_SHIFT
    }

    /// The zero-based slot index within the owning registry.
    pub(super) fn slot_index(self) -> usize {
        let slot_identity = (self.0 & SLOT_MASK) >> SLOT_SHIFT;
        // A valid id always has a one-based, non-zero slot identity, so
        // this subtraction is safe after `from_raw` validation.
        (slot_identity - 1) as usize
    }

    /// The slot generation (one-based).
    pub(super) const fn generation(self) -> u64 {
        (self.0 & GEN_MASK) >> GEN_SHIFT
    }
}

/// Allocates the next process-global registry tag.
///
/// Returns monotonically increasing tags starting at `1`. Once
/// [`MAX_REGISTRY_TAG`] has been handed out, every subsequent call returns
/// `OperationRegistryTagExhausted`. Uses [`Ordering::Relaxed`] because tags are
/// never compared across threads, only required to be unique.
pub(super) fn allocate_registry_tag() -> OperationResult<u64> {
    #[cfg(test)]
    let source = test_seam::source().unwrap_or(&NEXT_REGISTRY_TAG);
    #[cfg(not(test))]
    let source = &NEXT_REGISTRY_TAG;
    match source.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        // Hand out `current` (1..=MAX), advancing to `current + 1`; once
        // `current` exceeds `MAX_REGISTRY_TAG` the space is exhausted.
        if current <= MAX_REGISTRY_TAG {
            Some(current + 1)
        } else {
            None
        }
    }) {
        Ok(tag) => Ok(tag),
        Err(current) => Err(OperationError::new(
            OperationErrorCode::OperationRegistryTagExhausted,
            "vm::operation",
            "operation registry tag identity space is exhausted",
        )
        .with_limit(MAX_REGISTRY_TAG)
        .with_value(current)),
    }
}

/// Builds a packed id from structured fields.
///
/// * `registry_tag` must be in `1..=MAX_REGISTRY_TAG`;
/// * `slot_index` is a zero-based index and is converted to a one-based
///   identity with checked overflow, subject to `1..=MAX_SLOT_IDENTITY`;
/// * `generation` must be in `1..=MAX_GENERATION`.
///
/// Returns [`None`] for any out-of-bounds/overflowing input.
pub(super) fn encode(registry_tag: u64, slot_index: usize, generation: u64) -> Option<OperationId> {
    let slot_identity = u64::try_from(slot_index).ok()?.checked_add(1)?;

    if registry_tag == 0 || registry_tag > MAX_REGISTRY_TAG {
        return None;
    }
    if slot_identity > MAX_SLOT_IDENTITY {
        return None;
    }
    if generation == 0 || generation > MAX_GENERATION {
        return None;
    }

    let raw = (registry_tag << REG_TAG_SHIFT) | (slot_identity << SLOT_SHIFT) | generation;
    Some(OperationId(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference triple packing helper used to assert exact bit contents.
    fn pack(tag: u64, slot_identity: u64, generation: u64) -> u64 {
        (tag << REG_TAG_SHIFT) | (slot_identity << SLOT_SHIFT) | generation
    }

    #[test]
    fn minimum_id_roundtrips_and_is_positive() {
        let id = encode(1, 0, 1).expect("minimum id encodes");
        assert_eq!(id.registry_tag(), 1);
        assert_eq!(id.slot_index(), 0);
        assert_eq!(id.generation(), 1);
        let raw = id.raw();
        assert_eq!(raw, pack(1, 1, 1));
        assert!((raw as i64) > 0, "minimum id must be a positive i64");
        assert_eq!(OperationId::from_raw(raw).expect("decodes"), id);
    }

    #[test]
    fn maximum_id_roundtrips_and_is_positive() {
        let id = encode(
            MAX_REGISTRY_TAG,
            (MAX_SLOT_IDENTITY - 1) as usize,
            MAX_GENERATION,
        )
        .expect("maximum id encodes");
        assert_eq!(id.registry_tag(), MAX_REGISTRY_TAG);
        assert_eq!(id.slot_index(), (MAX_SLOT_IDENTITY - 1) as usize);
        assert_eq!(id.generation(), MAX_GENERATION);
        let raw = id.raw();
        assert_eq!(
            raw,
            pack(MAX_REGISTRY_TAG, MAX_SLOT_IDENTITY, MAX_GENERATION)
        );
        assert!((raw as i64) > 0, "maximum id must be a positive i64");
        assert_eq!(OperationId::from_raw(raw).expect("decodes"), id);
    }

    #[test]
    fn decode_rejects_zero() {
        let err = OperationId::from_raw(0).expect_err("zero must be rejected");
        assert_eq!(err.code(), OperationErrorCode::InvalidOperationId);
        assert_eq!(err.value(), Some(0));
    }

    #[test]
    fn decode_rejects_sign_bit() {
        // Valid fields plus the sign bit set.
        let raw = pack(1, 1, 1) | SIGN_MASK;
        let err = OperationId::from_raw(raw).expect_err("sign bit must be rejected");
        assert_eq!(err.code(), OperationErrorCode::InvalidOperationId);
        assert_eq!(err.value(), Some(raw));
    }

    #[test]
    fn decode_rejects_zero_registry_tag() {
        let raw = pack(0, 1, 1);
        let err = OperationId::from_raw(raw).expect_err("zero tag must be rejected");
        assert_eq!(err.code(), OperationErrorCode::InvalidOperationId);
    }

    #[test]
    fn decode_rejects_zero_slot_identity() {
        let raw = pack(1, 0, 1);
        let err = OperationId::from_raw(raw).expect_err("zero slot must be rejected");
        assert_eq!(err.code(), OperationErrorCode::InvalidOperationId);
    }

    #[test]
    fn decode_rejects_zero_generation() {
        let raw = pack(1, 1, 0);
        let err = OperationId::from_raw(raw).expect_err("zero generation must be rejected");
        assert_eq!(err.code(), OperationErrorCode::InvalidOperationId);
    }

    #[test]
    fn encode_rejects_out_of_range_fields() {
        assert!(encode(0, 0, 1).is_none(), "zero registry tag");
        assert!(encode(MAX_REGISTRY_TAG + 1, 0, 1).is_none(), "tag overflow");
        assert!(
            encode(1, (MAX_SLOT_IDENTITY) as usize, 1).is_none(),
            "slot overflow"
        );
        assert!(encode(1, usize::MAX, 1).is_none(), "slot index overflow");
        assert!(encode(1, 0, 0).is_none(), "zero generation");
        assert!(
            encode(1, 0, MAX_GENERATION + 1).is_none(),
            "generation overflow"
        );
    }

    #[test]
    fn distinct_fields_produce_distinct_ids() {
        let base = encode(7, 3, 9).expect("base id");

        let same_slot_tag = encode(8, 3, 9).expect("tag differs");
        assert_ne!(same_slot_tag, base);
        assert_ne!(same_slot_tag.registry_tag(), base.registry_tag());
        assert_eq!(same_slot_tag.slot_index(), base.slot_index());
        assert_eq!(same_slot_tag.generation(), base.generation());

        let same_tag_slot = encode(7, 4, 9).expect("slot differs");
        assert_ne!(same_tag_slot, base);
        assert_eq!(same_tag_slot.registry_tag(), base.registry_tag());
        assert_ne!(same_tag_slot.slot_index(), base.slot_index());
        assert_eq!(same_tag_slot.generation(), base.generation());

        let same_tag_gen = encode(7, 3, 10).expect("generation differs");
        assert_ne!(same_tag_gen, base);
        assert_eq!(same_tag_gen.registry_tag(), base.registry_tag());
        assert_eq!(same_tag_gen.slot_index(), base.slot_index());
        assert_ne!(same_tag_gen.generation(), base.generation());
    }

    #[test]
    fn explicit_inequality_with_different_fields() {
        // Same tag+slot, bumped generation must still unpack independently.
        let gen2 = encode(2, 5, 7).expect("a");
        let gen2_again = encode(2, 5, 8).expect("b");
        assert_ne!(gen2, gen2_again);
        assert_ne!(gen2.raw(), gen2_again.raw());
    }

    #[test]
    fn allocator_yields_distinct_nonzero_tags() {
        // Sample a bounded prefix only; deliberately do not exhaust the
        // global tag space.
        let mut tags = Vec::new();
        for _ in 0..64 {
            let tag = allocate_registry_tag().expect("tag allocated");
            assert_ne!(tag, 0, "tag must be nonzero");
            assert!(!tags.contains(&tag), "tag must not be reused: {tag}");
            tags.push(tag);
        }
        assert_eq!(tags.len(), 64);
    }

    #[test]
    fn registry_tag_allocator_repeated_post_max_failures_are_typed_and_monotonic() {
        use std::sync::atomic::AtomicU64;

        static COUNTER: AtomicU64 = AtomicU64::new(MAX_REGISTRY_TAG);
        let _source = test_seam::ScopedRegistryTagSource::install(&COUNTER);

        assert_eq!(
            allocate_registry_tag().expect("the maximum registry tag must hand out"),
            MAX_REGISTRY_TAG
        );
        let exhausted_value = MAX_REGISTRY_TAG + 1;
        for _ in 0..3 {
            let error = allocate_registry_tag().expect_err("registry tag space must be exhausted");
            assert_eq!(
                error.code(),
                OperationErrorCode::OperationRegistryTagExhausted
            );
            assert_eq!(error.limit(), Some(MAX_REGISTRY_TAG));
            assert_eq!(error.value(), Some(exhausted_value));
            assert_eq!(
                COUNTER.load(std::sync::atomic::Ordering::SeqCst),
                exhausted_value,
                "failed tag handouts must not advance, wrap, or reuse"
            );
        }
    }

    #[test]
    fn registry_tag_seam_is_scoped_and_independent_construction_recovers_after_drop() {
        use std::sync::atomic::AtomicU64;

        static COUNTER: AtomicU64 = AtomicU64::new(MAX_REGISTRY_TAG + 1);
        {
            let _source = test_seam::ScopedRegistryTagSource::install(&COUNTER);
            assert_eq!(
                allocate_registry_tag()
                    .expect_err("the installed exhausted source must fail")
                    .code(),
                OperationErrorCode::OperationRegistryTagExhausted
            );
        }

        let tag = allocate_registry_tag().expect("dropping the seam restores the real source");
        assert!(tag > 0);
    }
}
