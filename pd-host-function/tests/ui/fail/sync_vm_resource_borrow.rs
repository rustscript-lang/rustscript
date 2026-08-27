#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceRef;
use vm::{Vm, VmResult};

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A shared resource borrow cannot share a mutable VM parameter.
        #[pd_host_function(name = "test::sync_vm_resource_borrow")]
        fn sync_vm_resource_borrow(
            vm: &mut Vm,
            counter: ResourceRef<'_, Counter>,
        ) -> VmResult<i64> {
            let _ = (vm, counter);
            Ok(0)
        }
    }
}

fn main() {}
