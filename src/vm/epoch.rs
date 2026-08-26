use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochCheckpoint {
    deadline: Option<u64>,
    deadline_delta: u64,
    rearm_pending: bool,
    check_interval: u32,
    ops_until_check: u32,
}

impl EpochCheckpoint {
    pub fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    pub fn check_interval(&self) -> u32 {
        self.check_interval
    }
}

#[derive(Clone, Debug, Default)]
pub struct EpochHandle {
    current: Arc<AtomicU64>,
}

impl EpochHandle {
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    pub fn increment(&self) -> u64 {
        self.increment_by(1)
    }

    pub fn increment_by(&self, delta: u64) -> u64 {
        if delta == 0 {
            return self.current();
        }
        let previous = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(delta))
            })
            .unwrap_or_else(|current| current);
        previous.saturating_add(delta)
    }

    pub(super) fn as_ptr(&self) -> *const AtomicU64 {
        Arc::as_ptr(&self.current)
    }
}

impl Vm {
    #[inline(always)]
    pub(in crate::vm) fn charge_epoch_tick(&mut self) -> VmResult<()> {
        self.run_ctx.charge_epoch_tick()
    }

    #[inline(always)]
    pub(super) fn mark_interrupt_yield(&mut self, reason: VmYieldReason) {
        self.instance.last_yield_reason = Some(reason);
        if matches!(reason, VmYieldReason::Epoch) {
            self.run_ctx.epoch_rearm_pending = true;
        }
    }

    #[inline(always)]
    pub(super) fn rearm_epoch_after_yield_if_needed(&mut self) {
        if !self.run_ctx.epoch_rearm_pending {
            return;
        }
        if !self.epoch_interruption_enabled() {
            self.run_ctx.epoch_rearm_pending = false;
            return;
        }
        self.run_ctx.epoch_deadline = self
            .current_epoch()
            .saturating_add(self.run_ctx.epoch_deadline_delta);
        self.run_ctx.epoch_rearm_pending = false;
        self.reset_interrupt_countdown();
    }

    pub(super) fn clear_epoch_deadline_internal(&mut self) {
        self.run_ctx.clear_epoch_deadline_internal();
    }

    pub fn consume_epoch_tick(&mut self) -> VmResult<()> {
        if self.fuel_metering_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Epoch));
        }
        self.charge_epoch_tick()
    }

    pub fn epoch_handle(&self) -> EpochHandle {
        self.run_ctx.epoch_handle.clone()
    }

    pub fn current_epoch(&self) -> u64 {
        self.run_ctx.epoch_handle.current()
    }

    pub fn increment_epoch(&self) -> u64 {
        self.run_ctx.epoch_handle.increment()
    }

    pub fn increment_epoch_by(&self, delta: u64) -> u64 {
        self.run_ctx.epoch_handle.increment_by(delta)
    }

    pub fn set_epoch_deadline(&mut self, ticks_beyond_current: u64) -> VmResult<()> {
        if self.fuel_metering_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Epoch));
        }
        self.run_ctx.interrupt_mode = InterruptMode::Epoch;
        self.run_ctx.epoch_deadline = self.current_epoch().saturating_add(ticks_beyond_current);
        self.run_ctx.epoch_deadline_delta = ticks_beyond_current;
        self.run_ctx.epoch_rearm_pending = false;
        self.reset_interrupt_countdown();
        Ok(())
    }

    pub fn clear_epoch_deadline(&mut self) {
        self.clear_epoch_deadline_internal();
    }

    pub fn epoch_deadline(&self) -> Option<u64> {
        self.epoch_interruption_enabled()
            .then_some(self.run_ctx.epoch_deadline)
    }

    pub fn epoch_deadline_delta(&self) -> Option<u64> {
        self.epoch_interruption_enabled()
            .then_some(self.run_ctx.epoch_deadline_delta)
    }

    pub fn set_epoch_check_interval(&mut self, interval: u32) -> VmResult<()> {
        if interval == 0 {
            return Err(VmError::InvalidEpochCheckInterval(interval));
        }
        if self.fuel_metering_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Epoch));
        }
        self.run_ctx.fuel_check_interval = interval;
        self.reset_interrupt_countdown();
        Ok(())
    }

    pub fn epoch_check_interval(&self) -> u32 {
        self.fuel_check_interval()
    }

    pub fn epoch_checkpoint(&self) -> EpochCheckpoint {
        EpochCheckpoint {
            deadline: self
                .epoch_interruption_enabled()
                .then_some(self.run_ctx.epoch_deadline),
            deadline_delta: self.run_ctx.epoch_deadline_delta,
            rearm_pending: self.run_ctx.epoch_rearm_pending,
            check_interval: self.epoch_check_interval(),
            ops_until_check: self.run_ctx.fuel_ops_until_check,
        }
    }

    pub fn restore_epoch(&mut self, checkpoint: EpochCheckpoint) {
        self.clear_fuel_internal();
        self.run_ctx.interrupt_mode = if checkpoint.deadline.is_some() {
            InterruptMode::Epoch
        } else {
            InterruptMode::None
        };
        self.run_ctx.epoch_deadline = checkpoint.deadline.unwrap_or(0);
        self.run_ctx.epoch_deadline_delta = checkpoint.deadline_delta;
        self.run_ctx.epoch_rearm_pending = checkpoint.rearm_pending;
        self.run_ctx.fuel_check_interval = checkpoint.check_interval.max(1);
        self.run_ctx.fuel_ops_until_check = checkpoint
            .ops_until_check
            .clamp(1, self.run_ctx.fuel_check_interval);
    }

    pub fn last_yield_reason(&self) -> Option<VmYieldReason> {
        self.instance.last_yield_reason
    }
}
