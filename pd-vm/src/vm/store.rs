use super::{EpochCheckpoint, EpochHandle, FuelCheckpoint, Vm, VmResult, VmStatus};

/// Lightweight Wasmtime-style store wrapper for VM state and host context data.
pub struct Store<T = ()> {
    vm: Vm,
    data: T,
}

impl<T> Store<T> {
    pub fn new(vm: Vm, data: T) -> Self {
        Self { vm, data }
    }

    pub fn vm(&self) -> &Vm {
        &self.vm
    }

    pub fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    pub fn into_vm(self) -> Vm {
        self.vm
    }

    pub fn data(&self) -> &T {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut T {
        &mut self.data
    }

    pub fn into_data(self) -> T {
        self.data
    }

    pub fn run(&mut self) -> VmResult<VmStatus> {
        self.vm.run()
    }

    pub fn resume(&mut self) -> VmResult<VmStatus> {
        self.vm.resume()
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.vm.set_fuel(fuel);
    }

    pub fn clear_fuel(&mut self) {
        self.vm.clear_fuel();
    }

    pub fn set_fuel_check_interval(&mut self, interval: u32) -> VmResult<()> {
        self.vm.set_fuel_check_interval(interval)
    }

    pub fn fuel_check_interval(&self) -> u32 {
        self.vm.fuel_check_interval()
    }

    pub fn get_fuel(&self) -> Option<u64> {
        self.vm.get_fuel()
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.vm.add_fuel(fuel)
    }

    pub fn recharge(&mut self, fuel: u64) -> VmResult<()> {
        self.vm.recharge_fuel(fuel)
    }

    pub fn consume_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.vm.consume_fuel(fuel)
    }

    pub fn consume_fuel_tick(&mut self) -> VmResult<()> {
        self.vm.consume_fuel_tick()
    }

    pub fn epoch_handle(&self) -> EpochHandle {
        self.vm.epoch_handle()
    }

    pub fn current_epoch(&self) -> u64 {
        self.vm.current_epoch()
    }

    pub fn increment_epoch(&self) -> u64 {
        self.vm.increment_epoch()
    }

    pub fn increment_epoch_by(&self, delta: u64) -> u64 {
        self.vm.increment_epoch_by(delta)
    }

    pub fn set_epoch_deadline(&mut self, ticks_beyond_current: u64) -> VmResult<()> {
        self.vm.set_epoch_deadline(ticks_beyond_current)
    }

    pub fn clear_epoch_deadline(&mut self) {
        self.vm.clear_epoch_deadline();
    }

    pub fn epoch_deadline(&self) -> Option<u64> {
        self.vm.epoch_deadline()
    }

    pub fn epoch_deadline_delta(&self) -> Option<u64> {
        self.vm.epoch_deadline_delta()
    }

    pub fn set_epoch_check_interval(&mut self, interval: u32) -> VmResult<()> {
        self.vm.set_epoch_check_interval(interval)
    }

    pub fn epoch_check_interval(&self) -> u32 {
        self.vm.epoch_check_interval()
    }

    pub fn consume_epoch_tick(&mut self) -> VmResult<()> {
        self.vm.consume_epoch_tick()
    }

    pub fn epoch_checkpoint(&self) -> EpochCheckpoint {
        self.vm.epoch_checkpoint()
    }

    pub fn restore_epoch(&mut self, checkpoint: EpochCheckpoint) {
        self.vm.restore_epoch(checkpoint);
    }

    pub fn fuel_checkpoint(&self) -> FuelCheckpoint {
        self.vm.fuel_checkpoint()
    }

    pub fn checkpoint(&self) -> FuelCheckpoint {
        self.vm.checkpoint()
    }

    pub fn restore_fuel(&mut self, checkpoint: FuelCheckpoint) {
        self.vm.restore_fuel(checkpoint);
    }

    pub fn restore_checkpoint(&mut self, checkpoint: FuelCheckpoint) {
        self.vm.restore_checkpoint(checkpoint);
    }
}

impl Store<()> {
    pub fn from_vm(vm: Vm) -> Self {
        Self::new(vm, ())
    }
}
