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
//! * **Bounded, monotonic ids** — [`OperationRegistry`] allocates non-reusable
//!   [`OperationId`]s and bounds the number of concurrently *pending*
//!   operations; consuming a terminal result releases capacity.
//! * **Optional resource association** — an operation can be tied to a
//!   [`ResourceHandle`](crate::vm::resource::ResourceHandle)
//!   so cancelling that resource cancels the operation.
pub mod driver;
pub mod error;
pub mod reason;
pub mod registry;

pub use driver::{HostOperation, OperationCleanup, OperationOutcome, OperationSpec};
pub use error::{OperationError, OperationErrorCode, OperationResult};
pub use reason::OperationCancelReason;
pub use registry::{
    DEFAULT_MAX_PENDING_OPERATIONS, OperationId, OperationRegistry, OperationStatus,
};
