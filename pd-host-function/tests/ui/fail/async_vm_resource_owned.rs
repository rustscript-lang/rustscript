#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceOwned;
use vm::{Vm, VmResult};

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// Async host functions cannot borrow the VM while submitting a future.
        #[pd_host_function(name = "test::async_vm_resource_owned")]
        async fn async_vm_resource_owned(
            vm: &mut Vm,
            counter: ResourceOwned<Counter>,
        ) -> VmResult<i64> {
            let _ = (vm, counter);
            Ok(0)
        }
    }
}

fn main() {}
