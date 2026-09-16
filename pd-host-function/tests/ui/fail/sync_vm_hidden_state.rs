#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::host_api::HostState;
use vm::{HostStateMut, Vm, VmResult};

struct Tuning {
    value: i64,
}

impl HostState for Tuning {
    const KEY: &'static str = "ui.tuning";

    fn initialize() -> Result<Self, String> {
        Ok(Self { value: 0 })
    }
}

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A raw mutable VM borrow cannot share a wrapper with hidden state.
        #[pd_host_function(name = "test::bump_tuning_with_vm")]
        fn bump_tuning_with_vm(
            vm: &mut Vm,
            mut tuning: HostStateMut<'_, Tuning>,
        ) -> VmResult<i64> {
            let _ = vm;
            tuning.value += 1;
            Ok(tuning.value)
        }
    }
}

fn main() {}
