//! Run-level cancellation vocabulary and a slim run-flag token.
//!
//! The legacy operation/owner/poller machinery that once lived here has been
//! replaced by the modern [`crate::vm::operation`] layer: each in-flight host
//! operation is a concrete [`crate::vm::operation::HostOperation`] driver
//! registered in the single [`crate::vm::execution_scope::ExecutionScope`]
//! operation registry. The only pieces that remain here are:
//!
//! * the public [`CancellationReason`] vocabulary (re-exported at the VM
//!   boundary and still used by run context / invocation / host bridge /
//!   legacy resource cleanup), and
//! * a slim [`CancellationToken`] that records the *first* run-level
//!   cancellation reason as a plain flag. It is **not** a parent/child signal
//!   tree and propagates nothing: operation cancellation is delivered by the
//!   scope registry directly to each driver.
//!
//! There is deliberately no second operation registry, no `OperationOwner`
//! enum, and no static owner→poller table anywhere in this crate.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const REASON_MASK: u8 = 0x0F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CancellationReason {
    Requested = 1,
    Deadline = 2,
    VmReset = 3,
    Parent = 4,
    ResourceClosed = 5,
    /// The `Vm` itself was dropped while the work was pending.
    VmDrop = 6,
}

impl CancellationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Deadline => "deadline",
            Self::VmReset => "vm_reset",
            Self::Parent => "parent",
            Self::ResourceClosed => "resource_closed",
            Self::VmDrop => "vm_drop",
        }
    }

    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Requested),
            2 => Some(Self::Deadline),
            3 => Some(Self::VmReset),
            4 => Some(Self::Parent),
            5 => Some(Self::ResourceClosed),
            6 => Some(Self::VmDrop),
            _ => None,
        }
    }
}

struct TokenSignal {
    state: AtomicU8,
}

impl TokenSignal {
    fn cancel(&self, reason: CancellationReason) -> bool {
        self.state
            .compare_exchange(
                0,
                reason as u8 & REASON_MASK,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn reason(&self) -> Option<CancellationReason> {
        CancellationReason::from_raw(self.state.load(Ordering::Acquire))
    }
}

/// A slim, cloneable run-level cancellation flag.
///
/// The first [`cancel`](Self::cancel) call binds the reason; later cancels
/// (including conflicting reasons) are no-ops, so the first reason is
/// preserved. It never propagates to children and exposes no operation
/// registry; it is a pure run-scoped "bool + reason" marker consumed by the
/// invocation stream and the run-context reset path.
#[derive(Clone)]
pub struct CancellationToken {
    signal: Arc<TokenSignal>,
}

impl CancellationToken {
    pub(crate) fn root() -> Self {
        Self {
            signal: Arc::new(TokenSignal {
                state: AtomicU8::new(0),
            }),
        }
    }

    #[allow(dead_code)]
    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }

    pub fn reason(&self) -> Option<CancellationReason> {
        self.signal.reason()
    }

    pub fn cancel(&self, reason: CancellationReason) -> bool {
        self.signal.cancel(reason)
    }

    pub(crate) fn take_propagation_error(&self) -> Option<super::error::RuntimeError> {
        // No child propagation tree exists; there is never a propagation
        // error to take. Kept for the run-context `cancel` API shape.
        None
    }

    #[allow(dead_code)]
    pub fn check(&self) -> super::error::RuntimeResult<()> {
        let Some(reason) = self.reason() else {
            return Ok(());
        };
        Err(super::error::RuntimeError::new(
            super::error::RuntimeErrorCode::OperationCancelled,
            "runtime::operation",
            format!("operation was cancelled ({})", reason.as_str()),
        ))
    }
}
