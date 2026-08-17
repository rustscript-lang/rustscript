//! Host-agnostic generic operation layer.
//!
//! This module is the core, host-agnostic replacement for the old
//! builtin-specific operation machinery that lived in
//! `crate::builtins::runtime::cancellation`. It is the piece that will be
//! wired into the future `ExecutionScope` (fan-out across a scope is a
//! later step and deliberately not implemented here).
//!
//! Key ideas:
//!
//! * **Object-safe driver contract** — [`HostOperation::poll`]/[`HostOperation::cancel`]
//!   are owned by the concrete operation, not by an `OperationOwner` enum or a
//!   static poller table. The registry never dispatches on a host domain.
//! * **Single cancellation authority** — cancellation is recorded once on the
//!   operation entry (first reason wins, deadline included) and forwarded
//!   directly to the owning driver via [`HostOperation::cancel`]. There is no
//!   standalone [`CancellationToken`](crate::builtins::runtime::cancellation::CancellationToken)
//!   parent/child signal graph here and no second cancellation framework.
//! * **Bounded, monotonic ids** — [`OperationRegistry`] allocates non-reusable
//!   [`OperationId`]s and bounds the number of concurrently *pending*
//!   operations; consuming a terminal result releases capacity.
//! * **Optional resource association** — an operation can be tied to a
//!   [`ResourceHandle`](crate::builtins::runtime::resource::ResourceHandle)
//!   so cancelling that resource cancels the operation.
//!
//! # Transitional compatibility
//!
//! This branch keeps SQLite/IO/HTTP as standard builtins in the same `pd-vm`
//! crate and does **not** migrate them onto this layer yet. The old
//! `crate::builtins::runtime::cancellation` module remains as the transitional
//! compatibility surface so the concrete builtins and existing tests keep
//! compiling. Once integration happens:
//!
//! * delete the old `OperationOwner` enum, `RUNTIME_OPERATION_POLLERS` and the
//!   `owner` field on the old operation state;
//! * delete the old parent/child `CancellationToken` signal graph
//!   (`CancellationSignal`, `attach_parent`, children propagation) from
//!   `crate::builtins::runtime::cancellation`;
//! * delete the owner-based dispatch in
//!   `crate::builtins::runtime::poll_builtin_io_op` / `mod.rs`;
//! * route host bridge and builtin operations through this registry's
//!   [`OperationSpec`]/[`HostOperation`] contract instead of `start_owned`.

pub mod driver;
pub mod registry;

pub use driver::{HostOperation, OperationCleanup, OperationOutcome, OperationSpec};
pub use registry::{
    DEFAULT_MAX_PENDING_OPERATIONS, OperationId, OperationRegistry, OperationStatus,
};
