//! Host-agnostic generic operation layer.
//!
//! This module owns the host-agnostic operation lifecycle (status,
//! cancellation, cleanup) for the VM. The concrete driver contract lives
//! in [`driver`], the registry in [`registry`].
//!
//! Key ideas:
//!
//! * **Concrete driver owns poll/cancel** — each in-flight operation is a
//!   [`HostOperation`] that owns its own [`HostOperation::poll`] and
//!   [`HostOperation::cancel`] behaviour; the registry never dispatches on a
//!   host domain.
//! * **Registry owns per-entry reason/status** — the registry records the
//!   first cancellation reason (deadline included) and the terminal status on
//!   each operation entry, forwarding cancellation directly to the owning
//!   driver. There is no standalone cancellation-token graph and no second
//!   cancellation framework.
//! * **Packed, validated, reusable slots** — [`OperationRegistry`] stores
//!   operations in generational slots addressed by a packed registry-tag /
//!   slot-identity / generation [`OperationId`]. Caller-supplied ids are
//!   validated (foreign tag, out-of-range/future slot, or stale generation are
//!   rejected before any status/driver/cleanup mutation) and a released slot
//!   is reused under an incremented generation.

pub mod driver;
pub mod error;
pub mod id;
pub mod reason;
pub mod registry;

pub use driver::{HostOperation, OperationCleanup, OperationOutcome, OperationSpec};
pub use error::{OperationError, OperationErrorCode, OperationResult};
pub use id::OperationId;
pub use reason::OperationCancelReason;
pub use registry::{
    DEFAULT_MAX_PENDING_OPERATIONS, OperationCancelSummary, OperationRegistry, OperationStatus,
};
