use super::{FuelCheckpoint, Vm, VmResult, VmStatus};

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
