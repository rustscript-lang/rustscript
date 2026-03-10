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
        if !self.fuel_metering_enabled() {
            return 0;
        }
        let executed_since_last_check = self
            .fuel_check_interval
            .saturating_sub(self.fuel_ops_until_check);
        u64::from(executed_since_last_check)
    }

    #[inline(always)]
    pub(in crate::vm) fn charge_fuel(&mut self, amount: u64) -> VmResult<()> {
        if amount == 0 || !self.fuel_metering_enabled() {
            return Ok(());
        }

        let remaining = self.fuel_remaining;
        if remaining < amount {
            return Err(VmError::OutOfFuel {
                needed: amount,
                remaining,
            });
        }
        self.fuel_remaining = remaining - amount;
        Ok(())
    }

    #[inline(always)]
    pub(in crate::vm) fn charge_fuel_tick(&mut self) -> VmResult<()> {
        if !self.fuel_metering_enabled() {
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

    pub(super) fn clear_fuel_internal(&mut self) {
        if self.fuel_metering_enabled() {
            self.interrupt_mode = InterruptMode::None;
        }
        self.fuel_remaining = 0;
        self.reset_interrupt_countdown();
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.clear_epoch_deadline_internal();
        self.interrupt_mode = InterruptMode::Fuel;
        self.fuel_remaining = fuel;
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
        self.validate_native_aot_interrupt_interval(interval)?;
        self.fuel_check_interval = interval;
        self.reset_interrupt_countdown();
        Ok(())
    }

    pub fn fuel_check_interval(&self) -> u32 {
        if self.native_aot_interrupt_check_interval == Some(0) {
            0
        } else {
            self.fuel_check_interval
        }
    }

    pub fn aot_fuel_check_interval(&self) -> Option<u32> {
        self.native_aot_interrupt_check_interval
    }

    pub fn get_fuel(&self) -> Option<u64> {
        self.fuel_metering_enabled()
            .then_some(self.fuel_remaining.saturating_sub(self.pending_fuel_debt()))
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        if fuel == 0 {
            return Ok(());
        }
        if self.epoch_interruption_enabled() {
            return Err(self.interruption_mode_conflict(InterruptMode::Fuel));
        }
        self.fuel_remaining = if self.fuel_metering_enabled() {
            self.fuel_remaining
                .checked_add(fuel)
                .ok_or(VmError::FuelOverflow)?
        } else {
            self.interrupt_mode = InterruptMode::Fuel;
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
            remaining: self.fuel_metering_enabled().then_some(self.fuel_remaining),
            check_interval: self.fuel_check_interval(),
            ops_until_check: self.fuel_ops_until_check,
        }
    }

    pub fn checkpoint(&self) -> FuelCheckpoint {
        self.fuel_checkpoint()
    }

    pub fn restore_fuel(&mut self, checkpoint: FuelCheckpoint) {
        self.clear_epoch_deadline_internal();
        self.interrupt_mode = if checkpoint.remaining.is_some() {
            InterruptMode::Fuel
        } else {
            InterruptMode::None
        };
        self.fuel_remaining = checkpoint.remaining.unwrap_or(0);
        if self.native_aot_interrupt_check_interval == Some(0) {
            self.fuel_check_interval = 1;
            self.fuel_ops_until_check = 1;
            return;
        }
        self.fuel_check_interval = self
            .native_aot_interrupt_check_interval
            .unwrap_or(checkpoint.check_interval.max(1));
        self.fuel_ops_until_check = checkpoint
            .ops_until_check
            .clamp(1, self.fuel_check_interval);
    }

    pub fn restore_checkpoint(&mut self, checkpoint: FuelCheckpoint) {
        self.restore_fuel(checkpoint);
    }
}
