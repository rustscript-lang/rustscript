use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuelCheckpoint {
    remaining: Option<u64>,
    check_interval: u32,
    ops_until_check: u32,
}

impl FuelCheckpoint {
    pub fn fuel(&self) -> Option<u64> {
        self.remaining
    }

    pub fn check_interval(&self) -> u32 {
        self.check_interval
    }
}

impl Vm {
    pub(super) fn pending_fuel_debt(&self) -> u64 {
        self.run_ctx.pending_fuel_debt()
    }

    #[inline(always)]
    pub(in crate::vm) fn charge_fuel(&mut self, amount: u64) -> VmResult<()> {
        self.run_ctx.charge_fuel(amount)
    }

    #[inline(always)]
    pub(in crate::vm) fn charge_fuel_tick(&mut self) -> VmResult<()> {
        self.run_ctx.charge_fuel_tick()
    }

    pub(super) fn clear_fuel_internal(&mut self) {
        self.run_ctx.clear_fuel_internal();
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.clear_epoch_deadline_internal();
        self.run_ctx.interrupt_mode = InterruptMode::Fuel;
        self.run_ctx.fuel_remaining = fuel;
        self.reset_interrupt_countdown();
    }

    pub fn clear_fuel(&mut self) {
        self.clear_fuel_internal();
    }

    pub fn set_fuel_check_interval(&mut self, interval: u32) -> VmResult<()> {
        if interval == 0 {
            return Err(VmError::InvalidFuelCheckInterval(interval));
        }
        if self.epoch_interruption_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Fuel));
        }
        self.run_ctx.fuel_check_interval = interval;
        self.reset_interrupt_countdown();
        Ok(())
    }

    pub fn fuel_check_interval(&self) -> u32 {
        self.run_ctx.fuel_check_interval
    }

    pub fn get_fuel(&self) -> Option<u64> {
        self.fuel_metering_enabled().then_some(
            self.run_ctx
                .fuel_remaining
                .saturating_sub(self.pending_fuel_debt()),
        )
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        if fuel == 0 {
            return Ok(());
        }
        if self.epoch_interruption_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Fuel));
        }
        self.run_ctx.fuel_remaining = if self.fuel_metering_enabled() {
            self.run_ctx
                .fuel_remaining
                .checked_add(fuel)
                .ok_or(VmError::FuelOverflow)?
        } else {
            self.run_ctx.interrupt_mode = InterruptMode::Fuel;
            self.reset_interrupt_countdown();
            fuel
        };
        Ok(())
    }

    pub fn recharge_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.add_fuel(fuel)
    }

    pub fn consume_fuel(&mut self, fuel: u64) -> VmResult<()> {
        if self.epoch_interruption_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Fuel));
        }
        self.charge_fuel(fuel)
    }

    pub fn consume_fuel_tick(&mut self) -> VmResult<()> {
        if self.epoch_interruption_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Fuel));
        }
        self.charge_fuel_tick()
    }

    pub fn fuel_checkpoint(&self) -> FuelCheckpoint {
        FuelCheckpoint {
            remaining: self
                .fuel_metering_enabled()
                .then_some(self.run_ctx.fuel_remaining),
            check_interval: self.fuel_check_interval(),
            ops_until_check: self.run_ctx.fuel_ops_until_check,
        }
    }

    pub fn checkpoint(&self) -> FuelCheckpoint {
        self.fuel_checkpoint()
    }

    pub fn restore_fuel(&mut self, checkpoint: FuelCheckpoint) {
        self.clear_epoch_deadline_internal();
        self.run_ctx.interrupt_mode = if checkpoint.remaining.is_some() {
            InterruptMode::Fuel
        } else {
            InterruptMode::None
        };
        self.run_ctx.fuel_remaining = checkpoint.remaining.unwrap_or(0);
        self.run_ctx.fuel_check_interval = checkpoint.check_interval.max(1);
        self.run_ctx.fuel_ops_until_check = checkpoint
            .ops_until_check
            .clamp(1, self.run_ctx.fuel_check_interval);
    }

    pub fn restore_checkpoint(&mut self, checkpoint: FuelCheckpoint) {
        self.restore_fuel(checkpoint);
    }
}
