//! Run-scoped execution context.
//!
//! [`RunContext`] owns everything that belongs to one execution of a program:
//! the run-scoped invocation stream configuration (event limits), fuel and
//! epoch budgets, the interrupt mode, and the epoch counter handle. A fresh
//! logical run starts from a reset context; nothing here survives a reset
//! except the epoch handle identity (which is intentionally process-lifetime).
//!
//! The embedder-facing fuel/epoch APIs live on the VM facade (see
//! `crate::vm::fuel` and `crate::vm::epoch`) and delegate here; cancellation
//! of pending host operations lives in the facade because it crosses into
//! [`HostRuntime`](super::host_runtime::HostRuntime) state.

use crate::builtins::runtime::cancellation::{CancellationReason, CancellationToken};
use crate::builtins::runtime::context::RuntimeContext;
use crate::vm::VmResult;
use crate::vm::epoch::EpochHandle;

/// Run interruption mode: no budget, fuel metering, or epoch deadlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum InterruptMode {
    None = 0,
    Fuel = 1,
    Epoch = 2,
}

impl InterruptMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fuel => "fuel",
            Self::Epoch => "epoch",
        }
    }
}

/// Run-scoped configuration, budgets, deadlines, and interruption state.
///
/// Thread safety: `RunContext` is `!Sync` (mutable counters) and not shared;
/// one facade owns one context. Clone semantics: not `Clone` — a clone would
/// duplicate run-scoped state across runs.
pub(crate) struct RunContext {
    pub(crate) runtime_context: RuntimeContext,
    pub(crate) cancellation: CancellationToken,
    pub(crate) interrupt_mode: InterruptMode,
    pub(crate) fuel_remaining: u64,
    pub(crate) fuel_check_interval: u32,
    pub(crate) fuel_ops_until_check: u32,
    pub(crate) epoch_deadline: u64,
    pub(crate) epoch_deadline_delta: u64,
    pub(crate) epoch_rearm_pending: bool,
    pub(crate) epoch_handle: EpochHandle,
    // Native ABI mirror: the epoch counter address read by generated code.
    // Load-bearing for `crate::vm::native`; see `crate::vm::engine`.
    #[allow(dead_code)]
    pub(crate) epoch_counter_ptr: usize,
}

impl RunContext {
    /// Creates a fresh run context with default event limits and no budgets
    /// (interrupts disabled).
    pub(crate) fn new() -> Self {
        let epoch_handle = EpochHandle::default();
        let epoch_counter_ptr = epoch_handle.as_ptr() as usize;
        Self {
            runtime_context: RuntimeContext::default(),
            cancellation: CancellationToken::root(),
            interrupt_mode: InterruptMode::None,
            fuel_remaining: 0,
            fuel_check_interval: 1,
            fuel_ops_until_check: 1,
            epoch_deadline: 0,
            epoch_deadline_delta: 0,
            epoch_rearm_pending: false,
            epoch_handle,
            epoch_counter_ptr,
        }
    }

    /// Closes run-scoped state for reuse: fuel/epoch budgets are dropped
    /// (metering disabled, no leftovers). The invocation stream event limits
    /// are configuration and intentionally survive a reset.
    pub(crate) fn reset_for_reuse(&mut self) {
        self.cancellation.cancel(CancellationReason::VmReset);
        self.cancellation = CancellationToken::root();
        self.epoch_rearm_pending = false;
        self.clear_fuel_internal();
        self.clear_epoch_deadline_internal();
    }

    pub(crate) fn cancel(&self, reason: CancellationReason) -> VmResult<()> {
        self.cancellation.cancel(reason);
        match self.cancellation.take_propagation_error() {
            Some(error) => Err(crate::vm::VmError::HostError(error.to_string())),
            None => Ok(()),
        }
    }

    pub(crate) fn reset_interrupt_countdown(&mut self) {
        self.fuel_ops_until_check = self.fuel_check_interval.max(1);
    }

    pub(crate) fn clear_fuel_internal(&mut self) {
        if self.interrupt_mode == InterruptMode::Fuel {
            self.interrupt_mode = InterruptMode::None;
        }
        self.fuel_remaining = 0;
        self.reset_interrupt_countdown();
    }

    pub(crate) fn clear_epoch_deadline_internal(&mut self) {
        if self.interrupt_mode == InterruptMode::Epoch {
            self.interrupt_mode = InterruptMode::None;
        }
        self.epoch_deadline = 0;
        self.epoch_deadline_delta = 0;
        self.epoch_rearm_pending = false;
        self.reset_interrupt_countdown();
    }

    pub(crate) fn pending_fuel_debt(&self) -> u64 {
        if self.interrupt_mode != InterruptMode::Fuel {
            return 0;
        }
        let executed_since_last_check = self
            .fuel_check_interval
            .saturating_sub(self.fuel_ops_until_check);
        u64::from(executed_since_last_check)
    }

    /// Charges a fixed amount of fuel; errors when the budget is exhausted.
    pub(crate) fn charge_fuel(&mut self, amount: u64) -> VmResult<()> {
        if amount == 0 || self.interrupt_mode != InterruptMode::Fuel {
            return Ok(());
        }
        let remaining = self.fuel_remaining;
        if remaining < amount {
            return Err(crate::vm::VmError::OutOfFuel {
                needed: amount,
                remaining,
            });
        }
        self.fuel_remaining = remaining - amount;
        Ok(())
    }

    /// Charges one fuel interval according to the countdown; errors when the
    /// budget is exhausted.
    pub(crate) fn charge_fuel_tick(&mut self) -> VmResult<()> {
        if self.interrupt_mode != InterruptMode::Fuel {
            return Ok(());
        }
        if self.fuel_ops_until_check > 1 {
            self.fuel_ops_until_check -= 1;
            return Ok(());
        }
        let amount = u64::from(self.fuel_check_interval);
        self.charge_fuel(amount)?;
        self.fuel_ops_until_check = self.fuel_check_interval;
        Ok(())
    }

    /// Charges one epoch countdown tick; errors when the deadline passed.
    pub(crate) fn charge_epoch_tick(&mut self) -> VmResult<()> {
        if self.interrupt_mode != InterruptMode::Epoch {
            return Ok(());
        }
        if self.fuel_ops_until_check > 1 {
            self.fuel_ops_until_check -= 1;
            return Ok(());
        }
        let current = self.epoch_handle.current();
        if current >= self.epoch_deadline {
            return Err(crate::vm::VmError::EpochDeadlineReached {
                current,
                deadline: self.epoch_deadline,
            });
        }
        self.fuel_ops_until_check = self.fuel_check_interval;
        Ok(())
    }
}

impl Default for RunContext {
    fn default() -> Self {
        Self::new()
    }
}
